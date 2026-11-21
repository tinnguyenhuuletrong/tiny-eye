//! TQS 3 → DuckDB SQL. Strings from the query are always bound (`$n`); only numbers the
//! validator produced (epoch ms, durations) and validated identifiers are formatted in.
//!
//! Metric functions reproduce Prometheus 3 semantics so dashboards show the same numbers:
//! left-open windows `(t - w, t]`, 5 m lookback for `last`, the extrapolation rules of
//! `extrapolatedRate`, and `BucketQuantile` interpolation for `histogram_quantile`.

use super::formula::{self, Expr};
use super::*;
use anyhow::{Result, bail};
use duckdb::Connection;
use duckdb::types::Value;
#[derive(Default)]
struct Sql {
    params: Vec<Value>,
}

impl Sql {
    fn bind(&mut self, v: Value) -> String {
        self.params.push(v);
        format!("${}", self.params.len())
    }
    fn text(&mut self, s: &str) -> String {
        self.bind(Value::Text(s.to_string()))
    }
}

/// Validated label name → SQL string literal (the validator restricts it to [A-Za-z0-9_]).
fn lit(label: &str) -> String {
    format!("'{}'", label.replace('\'', "''"))
}

#[derive(Debug, Clone, PartialEq)]
pub struct OutRow {
    pub output: String,
    pub t: i64,
    pub key: String,
    pub value: f64,
}

/// Grouping signature: formulas may only combine vectors grouped the same way.
#[derive(Debug, Clone, PartialEq)]
enum Shape {
    Scalar,
    Vector(String),
}

fn query_shape(q: &NamedQuery) -> Shape {
    if q.metric.is_none() {
        // Spans/logs: one series unless grouped.
        return match q.agg.as_ref().and_then(|a| a.by.clone()) {
            Some(mut b) if !b.is_empty() => {
                b.sort();
                Shape::Vector(format!("by:{}", b.join(",")))
            }
            _ => Shape::Scalar,
        };
    }
    if let Some(t) = &q.then {
        return match t.by.clone() {
            Some(mut b) if !b.is_empty() => {
                b.sort();
                Shape::Vector(format!("by:{}", b.join(",")))
            }
            _ => Shape::Scalar,
        };
    }
    let by = q.agg.as_ref().and_then(|a| a.by.clone());
    let without = q.agg.as_ref().and_then(|a| a.without.clone());
    match (by, without) {
        (Some(b), _) if b.is_empty() => Shape::Scalar,
        (Some(mut b), _) => {
            b.sort();
            Shape::Vector(format!("by:{}", b.join(",")))
        }
        (_, Some(mut w)) => {
            w.sort();
            Shape::Vector(format!("without:{}", w.join(",")))
        }
        // agg with neither by nor without collapses everything into one series.
        _ if q.agg.is_some() => Shape::Scalar,
        _ => Shape::Vector(format!("series:{}", q.name)),
    }
}

fn shape_of(tqs: &Tqs, name: &str, seen: &mut Vec<String>) -> Option<Shape> {
    if let Some(q) = tqs.query(name) {
        return Some(query_shape(q));
    }
    let f = tqs.formulas.iter().find(|f| f.name == name)?;
    if seen.contains(&f.name) {
        return None;
    }
    seen.push(f.name.clone());
    let ast = formula::parse(&f.expr).ok()?;
    combined_shape(tqs, &ast, seen).ok()
}

fn combined_shape(tqs: &Tqs, ast: &Expr, seen: &mut Vec<String>) -> Result<Shape, String> {
    let mut vector: Option<Shape> = None;
    for r in ast.refs() {
        match shape_of(tqs, &r, seen) {
            Some(Shape::Scalar) | None => {}
            Some(v) => match &vector {
                None => vector = Some(v),
                Some(existing) if *existing == v => {}
                Some(existing) => {
                    return Err(format!(
                        "operands are grouped differently ({existing:?} vs {v:?}): group every vector operand by the same labels"
                    ));
                }
            },
        }
    }
    Ok(vector.unwrap_or(Shape::Scalar))
}

pub fn formula_shape(tqs: &Tqs, ast: &Expr) -> Result<(), String> {
    // Two raw per-series queries of different metrics never share label sets reliably.
    let vectors: Vec<String> = ast
        .refs()
        .into_iter()
        .filter(|r| matches!(shape_of(tqs, r, &mut vec![]), Some(Shape::Vector(s)) if s.starts_with("series:")))
        .collect();
    if vectors.len() > 1 {
        return Err(format!("{vectors:?} are ungrouped series of different queries; add agg.by to each"));
    }
    combined_shape(tqs, ast, &mut vec![]).map(|_| ())
}

fn key_expr(labels_col: &str, agg: Option<&Agg>, drop_le: bool) -> String {
    let entry = "lambda e: e.key || '=' || e.value";
    let le = if drop_le { " AND e.key <> 'le'" } else { "" };
    if agg.is_some_and(|a| a.by.is_none() && a.without.is_none()) {
        return "''".into();
    }
    match agg.and_then(|a| a.by.as_ref()) {
        Some(by) => {
            let mut by = by.clone();
            by.sort();
            by.dedup();
            let parts: Vec<String> = by
                .iter()
                .filter(|l| !(drop_le && *l == "le"))
                .map(|l| format!("CASE WHEN coalesce(map_extract_value({labels_col}, {k}), '') <> '' THEN {k} || '=' || map_extract_value({labels_col}, {k}) END", k = lit(l)))
                .collect();
            if parts.is_empty() { "''".into() } else { format!("concat_ws(',', {})", parts.join(", ")) }
        }
        None => {
            let without: Vec<String> = if mutant("without_ignored") {
                vec![]
            } else {
                agg.and_then(|a| a.without.clone()).unwrap_or_default().iter().map(|l| lit(l)).collect()
            };
            let excl = if without.is_empty() { String::new() } else { format!(" AND e.key NOT IN ({})", without.join(", ")) };
            format!(
                "array_to_string(list_sort(list_transform(list_filter(map_entries({labels_col}), lambda e: e.value <> ''{excl}{le}), {entry})), ',')"
            )
        }
    }
}

fn matcher_sql(sql: &mut Sql, m: &Matcher, vars: &Vars, r: &Resolved) -> String {
    let lv = format!("coalesce(map_extract_value(labels, {}), '')", lit(&m.label));
    let v = sql.text(&resolve_var(&m.value, vars, r).unwrap_or_default());
    match m.op.as_str() {
        "=" => format!("{lv} = {v}"),
        "!=" => format!("{lv} <> {v}"),
        "=~" => format!("regexp_full_match({lv}, {v})"),
        _ => format!("NOT regexp_full_match({lv}, {v})"),
    }
}

/// CTE body producing `(t, key, value)` for a metrics query.
type Vars = std::collections::BTreeMap<String, String>;

/// PromQL `label_replace` on the labels map: when `regex` fully matches `from`, set
/// `label` to the expanded replacement; otherwise leave the map unchanged.
fn derive_sql(sql: &mut Sql, labels: String, d: &Derive) -> String {
    let src = format!("coalesce(map_extract_value({labels}, {}), '')", lit(&d.from));
    let re = sql.text(&d.regex);
    let repl = sql.text(&Regex::new(r"\$([1-9])").expect("static regex").replace_all(&d.replacement, r"\$1"));
    format!(
        "CASE WHEN regexp_full_match({src}, {re}) THEN map_concat({labels}, map([{dst}], [regexp_replace({src}, '^(?:' || {re} || ')$', {repl})])) ELSE {labels} END",
        dst = lit(&d.label)
    )
}

fn lbl_expr(labels_col: &str, agg: Option<&Agg>) -> String {
    match agg {
        Some(Agg { by: Some(by), .. }) if !by.is_empty() => format!(
            "map([{}], [{}])",
            by.iter().map(|l| lit(l)).collect::<Vec<_>>().join(", "),
            by.iter().map(|l| format!("coalesce(map_extract_value({labels_col}, {}), '')", lit(l))).collect::<Vec<_>>().join(", ")
        ),
        Some(Agg { without: Some(w), .. }) => format!(
            "map_from_entries(list_filter(map_entries({labels_col}), lambda e: e.key NOT IN ({})))",
            w.iter().map(|l| lit(l)).collect::<Vec<_>>().join(", ")
        ),
        Some(_) => "MAP {}::MAP(VARCHAR, VARCHAR)".into(),
        None => labels_col.into(),
    }
}

fn agg_fn(op: Option<AggOp>) -> &'static str {
    match op {
        None => "any_value(value)",
        Some(AggOp::Sum) => "sum(value)",
        Some(AggOp::Avg) => "avg(value)",
        Some(AggOp::Min) => "min(value)",
        Some(AggOp::Max) => "max(value)",
        Some(AggOp::Count) | Some(AggOp::Absent) => "count(value)::DOUBLE",
    }
}

/// Per (step, series) window statistics: n, t1/v1 (first sample), t2/v2/st2 (last), corr
/// (reset correction), nresets, nchanges, pv_last/pt_last (sample before the last), and for the
/// `*_over_time` aggregates a/mn/mx/s. A window is `(t - o - w, t - o]`.
///
/// Fast path (everything except avg/min/max/sum_over_time): running sums per series, then the
/// first and last sample of each window by two ASOF joins — O(series × steps) instead of the
/// join path's O(series × steps × samples per window). The join path stays for the aggregates
/// whose value would otherwise come from a difference of large running sums.
fn per_cte(op: FnOp, o: i64, w: i64) -> String {
    let gt = if mutant("closed_window") { ">=" } else { ">" };
    let fast = !matches!(op, FnOp::AvgOverTime | FnOp::MinOverTime | FnOp::MaxOverTime | FnOp::SumOverTime);
    if fast {
        let corr = if mutant("no_reset_correction") { "0".to_string() } else { "l.c_corr - f.c_corr".to_string() };
        return format!(
            "cum AS (SELECT *, row_number() OVER w AS i,
                            sum(CASE WHEN v < pv THEN pv ELSE 0 END) OVER w AS c_corr,
                            sum(CASE WHEN v < pv THEN 1 ELSE 0 END) OVER w AS c_res,
                            sum(CASE WHEN v <> pv THEN 1 ELSE 0 END) OVER w AS c_chg
                     FROM pts WINDOW w AS (PARTITION BY series_id ORDER BY ts ROWS UNBOUNDED PRECEDING)),
             gs AS (SELECT g.t, s.series_id FROM grid g CROSS JOIN (SELECT DISTINCT series_id FROM pts) s),
             lastp AS (SELECT gs.t, gs.series_id, c.i, c.ts, c.v, c.st, c.pv, c.pt, c.c_corr, c.c_res, c.c_chg
                       FROM gs ASOF JOIN cum c ON gs.series_id = c.series_id AND c.ts <= gs.t - {o}),
             firstp AS (SELECT gs.t, gs.series_id, c.i, c.ts, c.v, c.c_corr, c.c_res, c.c_chg
                        FROM gs ASOF JOIN cum c ON gs.series_id = c.series_id AND c.ts {gt} gs.t - {o} - {w}),
             per AS (SELECT l.t, l.series_id, l.i - f.i + 1 AS n, f.ts AS t1, l.ts AS t2, f.v AS v1, l.v AS v2, l.st AS st2,
                            {corr} AS corr, l.c_res - f.c_res AS nresets, l.c_chg - f.c_chg AS nchanges,
                            CASE WHEN l.i > f.i THEN l.pv END AS pv_last, CASE WHEN l.i > f.i THEN l.pt END AS pt_last,
                            NULL::DOUBLE AS a, NULL::DOUBLE AS mn, NULL::DOUBLE AS mx, NULL::DOUBLE AS s
                     FROM lastp l JOIN firstp f USING (t, series_id) WHERE l.i >= f.i)"
        );
    }
    // The previous sample is inside the window (strict, as in the research compiler).
    let in_win = format!("pt > t - {o} - {w}");
    let corr =
        if mutant("no_reset_correction") { "0".to_string() } else { format!("coalesce(sum(pv) FILTER (WHERE v < pv AND {in_win}), 0)") };
    format!(
        "win AS (SELECT g.t, p.* FROM grid g JOIN pts p ON p.ts {gt} g.t - {o} - {w} AND p.ts <= g.t - {o}),
         per AS (SELECT t, series_id, count(*) AS n, min(ts) AS t1, max(ts) AS t2,
                        arg_min(v, ts) AS v1, arg_max(v, ts) AS v2, arg_max(st, ts) AS st2,
                        {corr} AS corr,
                        count(*) FILTER (WHERE v < pv AND {in_win}) AS nresets,
                        count(*) FILTER (WHERE v <> pv AND {in_win}) AS nchanges,
                        arg_max(pv, ts) FILTER (WHERE {in_win}) AS pv_last,
                        arg_max(pt, ts) FILTER (WHERE {in_win}) AS pt_last,
                        avg(v) AS a, min(v) AS mn, max(v) AS mx, sum(v) AS s
                 FROM win GROUP BY t, series_id)"
    )
}

fn metrics_cte(sql: &mut Sql, q: &NamedQuery, r: &Resolved, kind: Kind, vars: &Vars) -> String {
    let f = q.func.as_ref().expect("validated");
    let hist = f.op == FnOp::HistogramQuantile;
    let metric = format!("{}{}", q.metric.as_deref().expect("validated"), if hist { "_bucket" } else { "" });
    let mut wh = vec![format!("metric_name = {}", sql.text(&metric))];
    wh.extend(q.matchers.iter().map(|m| matcher_sql(sql, m, vars, r)));
    let w = if f.op == FnOp::Last {
        if mutant("lookback_4m") { 240_000 } else { LOOKBACK_MS }
    } else {
        f.window.as_deref().and_then(|w| resolve_var(w, vars, r)).as_deref().and_then(parse_duration_ms).expect("validated")
    };
    let labels = if mutant("derive_ignored") {
        "s.labels".to_string()
    } else {
        q.derive.iter().fold("s.labels".to_string(), |l, d| derive_sql(sql, l, d))
    };
    let o = q.offset.as_deref().and_then(parse_duration_ms).unwrap_or(0);
    let (start, end, step) = if kind == Kind::Table { (r.to_ms, r.to_ms, 1) } else { (r.from_ms, r.to_ms, r.step_ms) };
    let lo = start - o - w;
    let extrap = |is_counter: bool, is_rate: bool| -> String {
        let res = if is_counter { "(v2 - v1 + corr)" } else { "(v2 - v1)" };
        let zero = if is_counter {
            format!("CASE WHEN {res} > 0 AND v1 >= 0 THEN least(ds1, sampled * (v1 / {res})) ELSE ds1 END")
        } else {
            "ds1".into()
        };
        let per = if is_rate { format!(" / ({w} / 1000.0)") } else { String::new() };
        if mutant("no_extrapolation") {
            return format!("CASE WHEN n < 2 THEN NULL ELSE {res}{per} END");
        }
        format!(
            "CASE WHEN n < 2 THEN NULL ELSE {res} * ((sampled + ({zero}) + (CASE WHEN de0 >= avgd * 1.1 THEN avgd / 2 ELSE de0 END)) / sampled){per} END"
        )
    };
    let value = match f.op {
        FnOp::Rate | FnOp::HistogramQuantile => extrap(true, true),
        FnOp::Increase => extrap(true, false),
        FnOp::Delta => extrap(false, false),
        FnOp::Irate => "CASE WHEN n < 2 OR t2 = pt_last THEN NULL ELSE (CASE WHEN v2 < pv_last THEN v2 ELSE v2 - pv_last END) / ((t2 - pt_last) / 1000.0) END".into(),
        FnOp::Resets => "nresets::DOUBLE".into(),
        FnOp::Changes => "nchanges::DOUBLE".into(),
        // A staleness marker as the latest sample ends the series (Prometheus lookback rule).
        FnOp::Last => "CASE WHEN st2 THEN NULL ELSE v2 END".into(),
        FnOp::LastOverTime => "v2".into(),
        FnOp::AvgOverTime => "a".into(),
        FnOp::MinOverTime => "mn".into(),
        FnOp::MaxOverTime => "mx".into(),
        FnOp::SumOverTime => "s".into(),
        FnOp::CountOverTime => "n::DOUBLE".into(),
    };
    let per_series = format!(
        "sel AS (SELECT series_id, labels FROM metrics_series WHERE {where_}),
         pts AS (SELECT p.series_id, p.timestamp_ms AS ts, p.value AS v, p.is_stale AS st,
                        lag(p.value) OVER (PARTITION BY p.series_id ORDER BY p.timestamp_ms) AS pv,
                        lag(p.timestamp_ms) OVER (PARTITION BY p.series_id ORDER BY p.timestamp_ms) AS pt
                 FROM metrics_points p WHERE p.series_id IN (SELECT series_id FROM sel)
                   AND p.timestamp_ms > {lo} AND p.timestamp_ms <= {hi}{stale}),
         grid AS (SELECT unnest(generate_series({start}::BIGINT, {end}::BIGINT, {step}::BIGINT)) AS t),
         {per_cte},
         ex AS (SELECT *, (t2 - t1) / 1000.0 AS sampled,
                       CASE WHEN n > 1 THEN (t2 - t1) / 1000.0 / (n - 1) END AS avgd,
                       (t1 - (t - {o} - {w})) / 1000.0 AS ds0,
                       ((t - {o}) - t2) / 1000.0 AS de0 FROM per),
         ex2 AS (SELECT *, CASE WHEN ds0 >= avgd * 1.1 THEN avgd / 2 ELSE ds0 END AS ds1 FROM ex),
         ser AS (SELECT e.t, {labels} AS labels, {value} AS value FROM ex2 e JOIN sel s USING (series_id))",
        where_ = wh.join(" AND "),
        hi = end - o,
        // Range functions never see staleness markers; `last` needs them to end a series.
        stale = if f.op == FnOp::Last { "" } else { " AND NOT p.is_stale" },
        per_cte = per_cte(f.op, o, w),
    );
    let key = key_expr("labels", q.agg.as_ref(), hist);
    let body = if hist {
        let qv = f.q.expect("validated");
        let hub = if mutant("hist_upper_bound") { "true" } else { "false" };
        format!(
            "{per_series},
             hb AS (SELECT t, {key} AS key,
                           CASE WHEN map_extract_value(labels, 'le') = '+Inf' THEN 'inf'::DOUBLE ELSE TRY_CAST(map_extract_value(labels, 'le') AS DOUBLE) END AS le,
                           sum(value) AS c
                    FROM ser WHERE value IS NOT NULL GROUP BY ALL),
             hm AS (SELECT t, key, le, max(c) OVER (PARTITION BY t, key ORDER BY le ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS c FROM hb WHERE le IS NOT NULL),
             hl AS (SELECT *, lag(le) OVER w AS ple, lag(c) OVER w AS pc,
                           max(CASE WHEN isinf(le) THEN c END) OVER (PARTITION BY t, key) AS total,
                           count(*) OVER (PARTITION BY t, key) AS nb
                    FROM hm WINDOW w AS (PARTITION BY t, key ORDER BY le)),
             hq AS (SELECT t, key, any_value(total) AS total, any_value(nb) AS nb,
                           arg_min({{'le': le, 'c': c, 'ple': ple, 'pc': pc}}, le) FILTER (WHERE NOT isinf(le) AND c >= {qv} * total) AS b,
                           max(le) FILTER (WHERE NOT isinf(le)) AS second
                    FROM hl GROUP BY t, key),
             out AS (SELECT t, key,
                    CASE WHEN total IS NULL OR nb < 2 OR total = 0 THEN NULL
                         WHEN b IS NULL THEN second
                         WHEN b.ple IS NULL AND b.le <= 0 THEN b.le
                         WHEN {hub} THEN b.le
                         WHEN b.ple IS NULL THEN b.le * ({qv} * total / b.c)
                         ELSE b.ple + (b.le - b.ple) * (({qv} * total - b.pc) / (b.c - b.pc)) END AS value
                    FROM hq)"
        )
    } else if q.agg.as_ref().is_some_and(|a| a.op == Some(AggOp::Absent)) {
        // PromQL absent() labels its 1 with the query's equality matchers.
        let mut eq: Vec<&Matcher> = q.matchers.iter().filter(|m| m.op == "=" && !mutant("absent_unlabelled")).collect();
        eq.sort_by(|a, b| a.label.cmp(&b.label));
        let parts: Vec<String> = eq
            .iter()
            .map(|m| {
                let v = sql.text(&resolve_var(&m.value, vars, r).unwrap_or_default());
                format!("CASE WHEN {v} <> '' THEN {} || '=' || {v} END", lit(&m.label))
            })
            .collect();
        let key = if parts.is_empty() { "''".into() } else { format!("concat_ws(',', {})", parts.join(", ")) };
        format!(
            "{per_series}, out AS (SELECT g.t, {key} AS key, 1.0 AS value FROM grid g WHERE g.t NOT IN (SELECT t FROM ser WHERE value IS NOT NULL))"
        )
    } else {
        let agg = agg_fn(q.agg.as_ref().and_then(|a| a.op));
        let lbl = lbl_expr("labels", q.agg.as_ref());
        let first =
            format!("SELECT t, {key} AS key, any_value({lbl}) AS lbl, {agg} AS value FROM ser WHERE value IS NOT NULL GROUP BY t, key");
        // having filters the first aggregation; then aggregates what is left.
        let first = match &q.having {
            Some(h) if q.then.is_some() => apply_having(&first, Some(h)),
            _ => first,
        };
        match q.then.as_ref().filter(|_| !mutant("then_ignored")) {
            None => format!("{per_series}, out AS ({first})"),
            Some(t) => {
                let key2 = key_expr("lbl", Some(t), false);
                format!(
                    "{per_series}, a1 AS ({first}), out AS (SELECT t, {key2} AS key, {agg2} AS value FROM a1 GROUP BY t, {key2})",
                    agg2 = agg_fn(t.op)
                )
            }
        }
    };
    format!("WITH {body} SELECT t, key, value FROM out WHERE value IS NOT NULL")
}

fn ms_of(col: &str) -> String {
    format!("(epoch_ns({col}) // 1000000)")
}

fn cond_sql(sql: &mut Sql, c: &Cond, signal: &str) -> String {
    cond_sql_vars(sql, c, signal, &Vars::new(), None)
}

fn cond_sql_vars(sql: &mut Sql, c: &Cond, signal: &str, vars: &Vars, r: Option<&Resolved>) -> String {
    let resolved;
    let c = match (c.value.as_str(), r) {
        (Some(v), Some(r)) if v.starts_with('$') => {
            resolved = Cond { value: serde_json::Value::String(resolve_var(v, vars, r).unwrap_or_default()), ..c.clone() };
            &resolved
        }
        _ => c,
    };
    let attrs = if signal == "spans" { "span_attrs" } else { "log_attrs" };
    let col = if let Some(k) = c.field.strip_prefix("attr.") {
        format!("map_extract_value({attrs}, {})", sql.text(k))
    } else if let Some(k) = c.field.strip_prefix("resource.") {
        format!("map_extract_value(resource_attrs, {})", sql.text(k))
    } else if c.field == "duration_ms" {
        "(duration_ns / 1e6)".into()
    } else if c.field == "trace_id" {
        "lower(hex(trace_id))".into()
    } else {
        c.field.clone() // validated against SPAN_FIELDS / LOG_FIELDS
    };
    let bind = |sql: &mut Sql, v: &serde_json::Value| match v {
        serde_json::Value::Number(n) => sql.bind(Value::Double(n.as_f64().unwrap_or(f64::NAN))),
        serde_json::Value::Bool(b) => sql.bind(Value::Text(b.to_string())),
        other => sql.text(other.as_str().unwrap_or_default()),
    };
    match c.op.as_str() {
        "exists" => {
            let k = sql.text(c.field.split_once('.').map(|x| x.1).unwrap_or_default());
            let src = if c.field.starts_with("attr.") { attrs } else { "resource_attrs" };
            let e = format!("map_contains({src}, {k})");
            if c.value.as_bool() == Some(true) { e } else { format!("NOT {e}") }
        }
        "=~" => format!("regexp_full_match(coalesce({col}::VARCHAR, ''), {})", bind(sql, &c.value)),
        "!~" => format!("NOT regexp_full_match(coalesce({col}::VARCHAR, ''), {})", bind(sql, &c.value)),
        "contains" => format!("contains({col}, {})", bind(sql, &c.value)),
        "in" => {
            let vs: Vec<String> = c.value.as_array().map(|a| a.iter().map(|v| bind(sql, v)).collect()).unwrap_or_default();
            format!("{col} IN ({})", vs.join(", "))
        }
        op => format!("{col} {} {}", if op == "!=" { "<>" } else { op }, bind(sql, &c.value)),
    }
}

fn events_cte(sql: &mut Sql, q: &NamedQuery, r: &Resolved, kind: Kind, vars: &Vars) -> String {
    let (signal, table, ts, conds) = match (&q.spans, &q.logs) {
        (Some(c), _) => ("spans", "spans", ms_of("start_time_ns"), c),
        (_, Some(c)) => ("logs", "logs", ms_of("timestamp_ns"), c),
        _ => unreachable!("validated"),
    };
    let mut wh: Vec<String> = conds.iter().map(|c| cond_sql_vars(sql, c, signal, vars, Some(r))).collect();
    // Table: one bucket covering (from, to]. Timeseries: the point at t covers (t - step, t].
    let (start, end, step, width) =
        if kind == Kind::Table { (r.to_ms, r.to_ms, 1, r.to_ms - r.from_ms) } else { (r.from_ms, r.to_ms, r.step_ms, r.step_ms) };
    let gt = if mutant("bucket_closed_left") { ">=" } else { ">" };
    wh.push(format!("{ts} {gt} {} AND {ts} <= {end}", start - width));
    let by: Vec<String> = q.agg.as_ref().and_then(|a| a.by.clone()).unwrap_or_default();
    let key_parts: Vec<String> = by
        .iter()
        .map(|f| {
            let v = if let Some(k) = f.strip_prefix("attr.") {
                format!("map_extract_value({}, {})", if signal == "spans" { "span_attrs" } else { "log_attrs" }, sql.text(k))
            } else {
                format!("{f}::VARCHAR")
            };
            format!("CASE WHEN coalesce({v}, '') <> '' THEN {} || '=' || {v} END", lit(f))
        })
        .collect();
    let key = if key_parts.is_empty() { "''".into() } else { format!("concat_ws(',', {})", key_parts.join(", ")) };
    let m = q.measure.as_ref().expect("validated");
    let value = match m.op {
        MeasureOp::Count => "count(*)::DOUBLE".to_string(),
        MeasureOp::Rate => format!("count(*) / ({width} / 1000.0)"),
        MeasureOp::Quantile => format!("quantile_cont(duration_ns / 1e6, {})", m.q.expect("validated")),
        MeasureOp::Avg => "avg(duration_ns / 1e6)".into(),
        MeasureOp::Min => "min(duration_ns / 1e6)".into(),
        MeasureOp::Max => "max(duration_ns / 1e6)".into(),
        MeasureOp::Sum => "sum(duration_ns / 1e6)".into(),
    };
    format!(
        "WITH ev AS (SELECT {ts} AS ts, {key} AS key, * FROM {table} WHERE {}),
              grid AS (SELECT unnest(generate_series({start}::BIGINT, {end}::BIGINT, {step}::BIGINT)) AS t)
         SELECT g.t, ev.key, {value} AS value FROM grid g JOIN ev ON ev.ts {gt} g.t - {width} AND ev.ts <= g.t GROUP BY g.t, ev.key",
        wh.join(" AND ")
    )
}

fn apply_having(inner: &str, h: Option<&Having>) -> String {
    match h {
        None => inner.into(),
        Some(h) => format!(
            "SELECT * FROM ({inner}) WHERE value {} {:?}",
            if h.op == "=" {
                "="
            } else if h.op == "!=" {
                "<>"
            } else {
                &h.op
            },
            h.value
        ),
    }
}

fn apply_top(inner: &str, top: Option<&Top>) -> String {
    let Some(top) = top else { return inner.into() };
    let dir = if top.order == "asc" { "ASC" } else { "DESC" };
    if let Some(q) = top.query.as_ref().filter(|_| !mutant("top_query_ignored")) {
        return format!(
            "SELECT r.* FROM ({inner}) r SEMI JOIN (SELECT t, key FROM q_{q} QUALIFY row_number() OVER (PARTITION BY t ORDER BY value {dir}, key) <= {}) k USING (t, key)",
            top.n
        );
    }
    match top.rank.as_str() {
        "per_step" if top.query.is_some() => inner.into(),
        "per_step" => format!("SELECT * FROM ({inner}) QUALIFY row_number() OVER (PARTITION BY t ORDER BY value {dir}, key) <= {}", top.n),
        rank => {
            let score = match rank {
                "max" => "max(value)",
                "avg" => "avg(value)",
                _ => "arg_max(value, t)",
            };
            format!(
                "WITH r AS ({inner}), k AS (SELECT key FROM r GROUP BY key ORDER BY {score} {dir}, key LIMIT {})
                 SELECT r.* FROM r JOIN k USING (key)",
                top.n
            )
        }
    }
}

pub fn run(conn: &Connection, tqs: &Tqs, r: &Resolved) -> Result<Vec<OutRow>> {
    let (text, params) = timeseries_sql(tqs, r)?;
    let mut stmt = conn.prepare(&text)?;
    let rows = stmt
        .query_map(duckdb::params_from_iter(params.iter()), |row| {
            Ok(OutRow { output: row.get(0)?, t: row.get(1)?, key: row.get(2)?, value: row.get(3)? })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// The SQL text and bound parameters `run` executes (also used for EXPLAIN / profiling).
pub fn timeseries_sql(tqs: &Tqs, r: &Resolved) -> Result<(String, Vec<Value>)> {
    if !matches!(tqs.kind, Kind::Timeseries | Kind::Table) {
        bail!("run() handles timeseries/table; use run_list for {:?}", tqs.kind);
    }
    let mut sql = Sql::default();
    let mut ctes: Vec<String> = vec![];
    for q in &tqs.queries {
        let body = if q.metric.is_some() {
            metrics_cte(&mut sql, q, r, tqs.kind, &tqs.vars)
        } else {
            events_cte(&mut sql, q, r, tqs.kind, &tqs.vars)
        };
        let having = if q.then.is_some() { None } else { q.having.as_ref() };
        ctes.push(format!("q_{} AS ({})", q.name, apply_having(&body, having)));
    }
    let mut outputs: Vec<(String, String)> = vec![];
    let mut shapes: HashMap<String, Shape> = HashMap::new();
    for q in &tqs.queries {
        shapes.insert(q.name.clone(), query_shape(q));
        if q.fill == Fill::Zero && !mutant("fill_ignored") {
            let (start, end, step) = if tqs.kind == Kind::Table { (r.to_ms, r.to_ms, 1) } else { (r.from_ms, r.to_ms, r.step_ms) };
            // A scalar has exactly one key, so it is zero-filled even when nothing matched
            // (PromQL `x or vector(0)`); a vector can only fill the keys it has seen.
            let keys = if query_shape(q) == Shape::Scalar {
                "SELECT '' AS key".to_string()
            } else {
                format!("SELECT DISTINCT key FROM q_{}", q.name)
            };
            ctes.push(format!(
                "f_{n} AS (SELECT g.t, k.key, coalesce(q.value, 0) AS value
                   FROM (SELECT unnest(generate_series({start}::BIGINT, {end}::BIGINT, {step}::BIGINT)) AS t) g
                   CROSS JOIN ({keys}) k LEFT JOIN q_{n} q USING (t, key))",
                n = q.name
            ));
            if !q.hidden {
                outputs.push((q.name.clone(), format!("SELECT t, key, value FROM f_{}", q.name)));
            }
        } else if !q.hidden {
            outputs.push((q.name.clone(), format!("SELECT t, key, value FROM q_{}", q.name)));
        }
    }
    for f in &tqs.formulas {
        let ast = formula::parse(&f.expr).map_err(anyhow::Error::msg)?;
        let refs = ast.refs();
        let shape_of_ref = |n: &str| shapes.get(n).cloned().unwrap_or(Shape::Scalar);
        let outer = f.join.as_deref() == Some("outer") && !mutant("outer_as_inner");
        let fill_of = |n: &str| outer || tqs.query(n).is_some_and(|q| q.fill == Fill::Zero) && !mutant("fill_ignored");
        // A zero-filled query joins through its filled CTE so its empty steps exist.
        let src = |n: &str| {
            if tqs.query(n).is_some_and(|q| q.fill == Fill::Zero) && !mutant("fill_ignored") { format!("f_{n}") } else { format!("q_{n}") }
        };
        let refs: Vec<String> = refs.into_iter().filter(|n| n != "time").collect();
        let vectors: Vec<&String> = refs.iter().filter(|n| shape_of_ref(n) != Shape::Scalar).collect();
        let strict: Vec<&&String> = vectors.iter().filter(|n| !fill_of(n)).collect();
        let filled: Vec<&&String> = vectors.iter().filter(|n| fill_of(n)).collect();
        let scalars: Vec<&String> = refs.iter().filter(|n| shape_of_ref(n) == Shape::Scalar).collect();
        let mut from = String::new();
        if let Some(first) = strict.first() {
            from.push_str(&format!("(SELECT t, key, value AS v_{first} FROM {}) base", src(first)));
            for n in strict.iter().skip(1) {
                from.push_str(&format!(" JOIN (SELECT t, key, value AS v_{n} FROM {}) USING (t, key)", src(n)));
            }
        } else if !filled.is_empty() {
            let keys: Vec<String> = filled.iter().map(|n| format!("SELECT t, key FROM {}", src(n))).collect();
            from.push_str(&format!("({}) base", keys.join(" UNION ")));
        } else if let Some(first) = scalars.first() {
            from.push_str(&format!("(SELECT t, key, value AS v_{first} FROM {}) base", src(first)));
        }
        for n in &filled {
            from.push_str(&format!(" LEFT JOIN (SELECT t, key, value AS v_{n} FROM {}) USING (t, key)", src(n)));
        }
        for n in scalars.iter().skip(if strict.is_empty() && filled.is_empty() { 1 } else { 0 }) {
            let join = if fill_of(n) { "LEFT JOIN" } else { "JOIN" };
            from.push_str(&format!(" {join} (SELECT t, value AS v_{n} FROM {}) USING (t)", src(n)));
        }
        let expr = ast.to_sql(&|n: &str| {
            if n == "time" {
                if mutant("time_in_ms") { "(t * 1.0)".to_string() } else { "(t / 1000.0)".to_string() }
            } else if tqs.query(n).is_some_and(|q| q.fill == Fill::Zero) && !mutant("fill_ignored") {
                format!("coalesce(v_{n}, 0)")
            } else {
                format!("v_{n}")
            }
        });
        ctes.push(format!("q_{} AS (SELECT t, key, {expr} AS value FROM {from})", f.name));
        let shape = combined_shape(tqs, &ast, &mut vec![]).unwrap_or(Shape::Scalar);
        shapes.insert(f.name.clone(), shape);
        outputs.push((f.name.clone(), format!("SELECT t, key, value FROM q_{} WHERE value IS NOT NULL", f.name)));
    }
    let unions: Vec<String> =
        outputs.iter().map(|(name, s)| format!("SELECT {} AS output, * FROM ({})", lit(name), apply_top(s, tqs.top.as_ref()))).collect();
    let text = format!("WITH {} {} ORDER BY output, key, t", ctes.join(", "), unions.join(" UNION ALL "));
    Ok((text, sql.params))
}

/// List-shaped results (trace search, waterfall, log search, patterns): rows of named text cells.
pub fn run_list(conn: &Connection, tqs: &Tqs, r: &Resolved) -> Result<Vec<Vec<(String, String)>>> {
    let mut sql = Sql::default();
    let limit = tqs.limit.unwrap_or(100);
    let s = tqs.search.clone().unwrap_or_default();
    let text = match tqs.kind {
        Kind::Trace => {
            // `trace_id = unhex(..)` (not `lower(hex(trace_id)) = ..`) so DuckDB can prune files by
            // min/max and row groups by the bloom filter (E4). An optional time range bounds it further.
            let id = sql.text(tqs.trace_id.as_deref().unwrap_or_default());
            let range = if tqs.time.is_some() {
                format!(" AND {ts} > {} AND {ts} <= {}", r.from_ms, r.to_ms, ts = ms_of("start_time_ns"))
            } else {
                String::new()
            };
            format!(
                "WITH RECURSIVE s AS (SELECT * FROM spans WHERE trace_id = unhex({id}){range}),
                 tree(span_id, depth) AS (
                   SELECT span_id, 0 FROM s WHERE parent_span_id IS NULL OR parent_span_id NOT IN (SELECT span_id FROM s)
                   UNION ALL SELECT c.span_id, t.depth + 1 FROM s c JOIN tree t ON c.parent_span_id = t.span_id WHERE t.depth < 64),
                 kids AS (SELECT parent_span_id AS span_id, sum(duration_ns) AS child_ns FROM s GROUP BY 1),
                 t0 AS (SELECT min(start_time_ns) AS t0 FROM s)
                 SELECT tree.depth, s.region, s.service_name, s.span_name,
                        ((epoch_ns(s.start_time_ns) - epoch_ns(t0.t0)) / 1e6)::DECIMAL(12,3) AS offset_ms,
                        (s.duration_ns / 1e6)::DECIMAL(12,3) AS duration_ms,
                        (greatest(s.duration_ns::BIGINT - coalesce(kids.child_ns, 0)::BIGINT, 0) / 1e6)::DECIMAL(12,3) AS self_ms,
                        s.status_code, (s.parent_span_id IS NULL) AS is_root,
                        (s.parent_span_id IS NOT NULL AND s.parent_span_id NOT IN (SELECT span_id FROM s)) AS orphan
                 FROM s JOIN tree USING (span_id) LEFT JOIN kids USING (span_id), t0
                 ORDER BY s.start_time_ns, tree.depth"
            )
        }
        Kind::TraceSearch => {
            let span_match: Vec<String> = s.spans.iter().map(|c| cond_sql(&mut sql, c, "spans")).collect();
            let matched = if span_match.is_empty() { "true".into() } else { format!("bool_or({})", span_match.join(" AND ")) };
            let mut trace_conds = vec![];
            for c in &s.trace {
                let col = match c.field.as_str() {
                    "root.service_name" => "root_service",
                    "root.span_name" => "root_span",
                    other => other,
                };
                trace_conds.push(match (c.field.as_str(), c.op.as_str()) {
                    ("service", "=") => format!("list_contains(services, {})", sql.text(c.value.as_str().unwrap_or_default())),
                    ("service", "=~") => format!(
                        "len(list_filter(services, lambda x: regexp_full_match(x, {}))) > 0",
                        sql.text(c.value.as_str().unwrap_or_default())
                    ),
                    ("has_error", _) => format!("has_error = {}", c.value.as_bool().unwrap_or(false)),
                    (_, op @ ("=" | "!=" | ">" | ">=" | "<" | "<=")) => {
                        let v = match &c.value {
                            serde_json::Value::Number(n) => sql.bind(Value::Double(n.as_f64().unwrap_or(f64::NAN))),
                            other => sql.text(other.as_str().unwrap_or_default()),
                        };
                        format!("{col} {} {v}", if op == "!=" { "<>" } else { op })
                    }
                    (_, "=~") => format!("regexp_full_match(coalesce({col}, ''), {})", sql.text(c.value.as_str().unwrap_or_default())),
                    _ => "false".into(),
                });
            }
            let order = match s.order.as_deref() {
                Some("start_desc") => "start_ms DESC",
                _ => "duration_ms DESC",
            };
            format!(
                "WITH sp AS (SELECT * FROM spans WHERE {ts} > {f} AND {ts} <= {to}),
                 tr AS (SELECT trace_id, min({ts}) AS start_ms,
                              (max(epoch_ns(start_time_ns) + duration_ns) - min(epoch_ns(start_time_ns))) / 1e6 AS duration_ms,
                              count(*) AS span_count, bool_or(status_code = 2) AS has_error,
                              arg_min(service_name, start_time_ns) FILTER (WHERE parent_span_id IS NULL) AS root_service,
                              arg_min(span_name, start_time_ns) FILTER (WHERE parent_span_id IS NULL) AS root_span,
                              list_sort(list_distinct(list(service_name))) AS services,
                              {matched} AS matched
                       FROM sp GROUP BY trace_id)
                 SELECT lower(hex(trace_id)) AS trace_id, start_ms, round(duration_ms, 3) AS duration_ms, span_count, has_error,
                        coalesce(root_service, '(missing root)') AS root_service, coalesce(root_span, '') AS root_span, array_to_string(services, ',') AS services
                 FROM tr WHERE matched {extra}
                 ORDER BY {order}, trace_id LIMIT {limit}",
                ts = ms_of("start_time_ns"),
                f = r.from_ms,
                to = r.to_ms,
                extra = trace_conds.iter().map(|c| format!("AND {c}")).collect::<Vec<_>>().join(" ")
            )
        }
        Kind::LogSearch | Kind::LogPatterns => {
            let mut wh: Vec<String> = s.logs.iter().map(|c| cond_sql(&mut sql, c, "logs")).collect();
            let ts = ms_of("timestamp_ns");
            wh.push(format!("{ts} > {} AND {ts} <= {}", r.from_ms, r.to_ms));
            if tqs.kind == Kind::LogSearch {
                let dir = if s.order.as_deref() == Some("asc") { "ASC" } else { "DESC" };
                format!(
                    "SELECT {ts} AS ts_ms, service_name, severity_text, body, coalesce(lower(hex(trace_id)), '') AS trace_id
                     FROM logs WHERE {} ORDER BY timestamp_ns {dir}, body LIMIT {limit}",
                    wh.join(" AND ")
                )
            } else {
                format!(
                    "SELECT pattern_hash::VARCHAR AS pattern_hash, count(*) AS n, min(body) AS example, list_sort(list_distinct(list(service_name)))::VARCHAR AS services
                     FROM logs WHERE {} GROUP BY pattern_hash ORDER BY n DESC, pattern_hash LIMIT {limit}",
                    wh.join(" AND ")
                )
            }
        }
        _ => bail!("run_list handles list kinds only"),
    };
    let mut stmt = conn.prepare(&text)?;
    let mut rows = stmt.query(duckdb::params_from_iter(sql.params.iter()))?;
    let names: Vec<String> = rows.as_ref().map(|s| s.column_names()).unwrap_or_default();
    let mut out = vec![];
    while let Some(row) = rows.next()? {
        let mut cells = vec![];
        for (i, n) in names.iter().enumerate() {
            let v: Value = row.get(i)?;
            let text = match v {
                Value::Null => String::new(),
                Value::Text(t) => t,
                Value::Boolean(b) => b.to_string(),
                Value::Decimal(d) => d.to_string(),
                other => {
                    format!("{other:?}").trim_start_matches(|c: char| c.is_alphabetic()).trim_matches(|c| c == '(' || c == ')').to_string()
                }
            };
            cells.push((n.clone(), text));
        }
        out.push(cells);
    }
    Ok(out)
}

use regex::Regex;
use std::collections::HashMap;
