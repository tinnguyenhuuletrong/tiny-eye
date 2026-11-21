//! The TQS coverage suite. Three kinds of evidence:
//! 1. metrics cases: TQS result vs Prometheus 3 on the same fixtures, point by point;
//! 2. spans/logs/trace cases: TQS result vs an independent Rust computation over the fixtures;
//! 3. validation cases: queries that must be refused, each with the problem code it must carry.

use crate::fixtures::{self, Log, T0};
use crate::promref::{Points, Prom};
use crate::synth::Span;
use crate::tqs::{self, Tqs, compile};
use anyhow::{Context, Result, anyhow};
use duckdb::Connection;
use serde_json::{Value as J, json};
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::time::Instant;

const FROM: i64 = T0 + 30 * 60_000;
const TO: i64 = T0 + 115 * 60_000;
const STEP: i64 = 60_000;
const REL_TOL: f64 = 1e-9;

pub struct CaseResult {
    pub id: String,
    pub area: &'static str,
    pub function: String,
    pub reference: String,
    pub pass: bool,
    pub detail: String,
    pub ms: f64,
}

fn time(from: i64, to: i64, step: i64) -> J {
    json!({"from": from, "to": to, "step": format!("{}s", step / 1000)})
}

fn ts(queries: J, formulas: J) -> J {
    json!({"tqs": "3", "id": "case", "kind": "timeseries", "time": time(FROM, TO, STEP), "queries": queries, "formulas": formulas})
}

fn mq(name: &str, metric: &str, matchers: J, f: J, agg: J) -> J {
    let mut q = json!({"name": name, "metric": metric, "match": matchers, "fn": f});
    if !agg.is_null() {
        q["agg"] = agg;
    }
    q
}

fn m(label: &str, op: &str, value: &str) -> J {
    json!({"label": label, "op": op, "value": value})
}

const CALLS: &str = "margince_ai_calls_total";
const LAT: &str = "margince_ai_call_duration_seconds";
const QUEUE: &str = "margince_job_queue_depth";

struct MetricCase {
    id: &'static str,
    function: &'static str,
    tqs: J,
    output: &'static str,
    promql: String,
    instant: bool,
}

fn metric_cases() -> Vec<MetricCase> {
    let rate5 = json!({"op": "rate", "window": "5m"});
    let none = J::Null;
    let c = |id, function, tqs, output, promql: &str| MetricCase { id, function, tqs, output, promql: promql.to_string(), instant: false };
    let single = |id, function, q: J, promql: &str| MetricCase { id, function, tqs: ts(json!([q]), json!([])), output: "a", promql: promql.to_string(), instant: false };
    let mut v = vec![
        single("M01", "rate per series", mq("a", CALLS, json!([]), rate5.clone(), none.clone()), "rate(margince_ai_calls_total[5m])"),
        single("M02", "sum by of rate", mq("a", CALLS, json!([]), rate5.clone(), json!({"op": "sum", "by": ["provider"]})), "sum by (provider) (rate(margince_ai_calls_total[5m]))"),
        single("M03", "increase over 1h", mq("a", CALLS, json!([]), json!({"op": "increase", "window": "1h"}), json!({"op": "sum", "by": ["task"]})), "sum by (task) (increase(margince_ai_calls_total[1h]))"),
        single("M04", "irate", mq("a", CALLS, json!([]), json!({"op": "irate", "window": "1m"}), none.clone()), "irate(margince_ai_calls_total[1m])"),
        c(
            "M05",
            "error ratio (formula)",
            ts(
                json!([mq("err", CALLS, json!([m("outcome", "=", "error")]), rate5.clone(), json!({"op": "sum", "by": ["provider"]})), mq("all", CALLS, json!([]), rate5.clone(), json!({"op": "sum", "by": ["provider"]}))]),
                json!([{"name": "ratio", "expr": "err / all"}]),
            ),
            "ratio",
            "sum by (provider) (rate(margince_ai_calls_total{outcome=\"error\"}[5m])) / sum by (provider) (rate(margince_ai_calls_total[5m]))",
        ),
        single("M06", "histogram p95 by label", mq("a", LAT, json!([]), json!({"op": "histogram_quantile", "q": 0.95, "window": "5m"}), json!({"op": "sum", "by": ["task"]})), "histogram_quantile(0.95, sum by (task, le) (rate(margince_ai_call_duration_seconds_bucket[5m])))"),
        single("M07", "histogram p99 overall", mq("a", LAT, json!([]), json!({"op": "histogram_quantile", "q": 0.99, "window": "5m"}), json!({"op": "sum"})), "histogram_quantile(0.99, sum by (le) (rate(margince_ai_call_duration_seconds_bucket[5m])))"),
        single("M08", "histogram p99 per series (+Inf bucket edge)", mq("a", LAT, json!([]), json!({"op": "histogram_quantile", "q": 0.99, "window": "5m"}), none.clone()), "histogram_quantile(0.99, rate(margince_ai_call_duration_seconds_bucket[5m]))"),
        c(
            "M09",
            "average latency from _sum/_count",
            ts(
                json!([mq("s", "margince_ai_call_duration_seconds_sum", json!([]), rate5.clone(), json!({"op": "sum", "by": ["task"]})), mq("n", "margince_ai_call_duration_seconds_count", json!([]), rate5.clone(), json!({"op": "sum", "by": ["task"]}))]),
                json!([{"name": "avg_s", "expr": "s / n"}]),
            ),
            "avg_s",
            "sum by (task) (rate(margince_ai_call_duration_seconds_sum[5m])) / sum by (task) (rate(margince_ai_call_duration_seconds_count[5m]))",
        ),
        single("M10", "latest gauge value (5m lookback, stale series)", mq("a", QUEUE, json!([]), json!({"op": "last"}), none.clone()), "margince_job_queue_depth"),
        single("M11", "max across series", mq("a", QUEUE, json!([]), json!({"op": "last"}), json!({"op": "max"})), "max(margince_job_queue_depth)"),
        single("M12", "max_over_time", mq("a", QUEUE, json!([]), json!({"op": "max_over_time", "window": "10m"}), none.clone()), "max_over_time(margince_job_queue_depth[10m])"),
        single("M13", "avg_over_time", mq("a", QUEUE, json!([]), json!({"op": "avg_over_time", "window": "10m"}), none.clone()), "avg_over_time(margince_job_queue_depth[10m])"),
        single("M14", "min_over_time", mq("a", QUEUE, json!([]), json!({"op": "min_over_time", "window": "10m"}), none.clone()), "min_over_time(margince_job_queue_depth[10m])"),
        single("M15", "sum_over_time", mq("a", "margince_pgxpool_conns", json!([]), json!({"op": "sum_over_time", "window": "10m"}), none.clone()), "sum_over_time(margince_pgxpool_conns[10m])"),
        single("M16", "count_over_time", mq("a", QUEUE, json!([]), json!({"op": "count_over_time", "window": "10m"}), none.clone()), "count_over_time(margince_job_queue_depth[10m])"),
        single("M17", "last_over_time", mq("a", QUEUE, json!([]), json!({"op": "last_over_time", "window": "10m"}), none.clone()), "last_over_time(margince_job_queue_depth[10m])"),
        single("M18", "delta of a gauge", mq("a", QUEUE, json!([]), json!({"op": "delta", "window": "10m"}), none.clone()), "delta(margince_job_queue_depth[10m])"),
        MetricCase {
            id: "M19",
            function: "top 3 per step",
            tqs: {
                let mut t = ts(json!([mq("a", CALLS, json!([]), rate5.clone(), json!({"op": "sum", "by": ["provider", "task"]}))]), json!([]));
                t["top"] = json!({"n": 3, "rank": "per_step"});
                t
            },
            output: "a",
            promql: "topk(3, sum by (provider, task) (rate(margince_ai_calls_total[5m])))".into(),
            instant: false,
        },
        single("M20", "time shift (offset)", { let mut q = mq("a", CALLS, json!([]), rate5.clone(), json!({"op": "sum"})); q["offset"] = json!("30m"); q }, "sum(rate(margince_ai_calls_total[5m] offset 30m))"),
        single("M21", "regex and negative regex matchers", mq("a", CALLS, json!([m("provider", "=~", "anth.*"), m("task", "!~", "summ.*")]), rate5.clone(), json!({"op": "sum", "by": ["task"]})), "sum by (task) (rate(margince_ai_calls_total{provider=~\"anth.*\",task!~\"summ.*\"}[5m]))"),
        single("M22", "aggregate without labels", mq("a", CALLS, json!([]), rate5.clone(), json!({"op": "sum", "without": ["outcome", "tier", "model"]})), "sum without (outcome, tier, model) (rate(margince_ai_calls_total[5m]))"),
        single("M23", "count series by label", mq("a", CALLS, json!([]), json!({"op": "last"}), json!({"op": "count", "by": ["provider"]})), "count by (provider) (margince_ai_calls_total)"),
        single("M24", "counter resets", mq("a", CALLS, json!([]), json!({"op": "resets", "window": "30m"}), none.clone()), "resets(margince_ai_calls_total[30m])"),
        single("M25", "gauge changes", mq("a", QUEUE, json!([]), json!({"op": "changes", "window": "10m"}), none.clone()), "changes(margince_job_queue_depth[10m])"),
        c("M26", "scalar arithmetic (per minute)", ts(json!([mq("a", CALLS, json!([]), rate5.clone(), json!({"op": "sum"}))]), json!([{"name": "per_min", "expr": "a * 60"}])), "per_min", "sum(rate(margince_ai_calls_total[5m])) * 60"),
        single("M27", "value filter (having)", { let mut q = mq("a", CALLS, json!([]), rate5.clone(), json!({"op": "sum", "by": ["task"]})); q["having"] = json!({"op": ">", "value": 1.0}); q }, "sum by (task) (rate(margince_ai_calls_total[5m])) > 1"),
        single("M28", "label value containing a quote", mq("a", CALLS, json!([m("task", "=", "quote\"d")]), rate5.clone(), json!({"op": "sum"})), "sum(rate(margince_ai_calls_total{task=\"quote\\\"d\"}[5m]))"),
        single("M29", "avg across series", mq("a", CALLS, json!([]), rate5.clone(), json!({"op": "avg", "by": ["provider"]})), "avg by (provider) (rate(margince_ai_calls_total[5m]))"),
        MetricCase {
            id: "M30",
            function: "instant table",
            tqs: json!({"tqs": "3", "id": "case", "kind": "table", "time": {"from": TO - 3_600_000, "to": TO}, "queries": [mq("a", CALLS, json!([]), json!({"op": "increase", "window": "1h"}), json!({"op": "sum", "by": ["provider"]}))]}),
            output: "a",
            promql: "sum by (provider) (increase(margince_ai_calls_total[1h]))".into(),
            instant: true,
        },
        single("M31", "window shorter than scrape interval", mq("a", CALLS, json!([]), json!({"op": "rate", "window": "10s"}), none.clone()), "rate(margince_ai_calls_total[10s])"),
        c("M32", "clamp", ts(json!([mq("a", CALLS, json!([]), rate5.clone(), json!({"op": "sum"}))]), json!([{"name": "c", "expr": "clamp(a, 0, 20)"}])), "c", "clamp(sum(rate(margince_ai_calls_total[5m])), 0, 20)"),
        c(
            "M33",
            "formula of a formula",
            ts(
                json!([mq("err", CALLS, json!([m("outcome", "=", "error")]), rate5.clone(), json!({"op": "sum", "by": ["provider"]})), mq("all", CALLS, json!([]), rate5.clone(), json!({"op": "sum", "by": ["provider"]}))]),
                json!([{"name": "ratio", "expr": "err / all"}, {"name": "pct", "expr": "ratio * 100"}]),
            ),
            "pct",
            "sum by (provider) (rate(margince_ai_calls_total{outcome=\"error\"}[5m])) / sum by (provider) (rate(margince_ai_calls_total[5m])) * 100",
        ),
    ];
    v.retain(|c| !c.promql.is_empty());
    v
}

/// Real panels of the Margince Grafana board (PromQL recorded in margince-qc's
/// `shared/stag-dashboards.csv`), each hand-translated to TQS. Grafana substitutes
/// `$namespace` and `$__range`; the PromQL here is the panel text after that substitution.
fn dashboard_cases() -> Vec<(String, String, J, &'static str, String, bool)> {
    let range_s = (TO - FROM) / 1000;
    let ns_m = || m("kubernetes_namespace", "=", "$namespace");
    let table = |vars: J, queries: J, formulas: J| json!({"tqs": "3", "id": "panel", "kind": "table", "time": {"from": FROM, "to": TO}, "vars": vars, "queries": queries, "formulas": formulas});
    let graph = |vars: J, queries: J, formulas: J, top: J| {
        let mut j = json!({"tqs": "3", "id": "panel", "kind": "timeseries", "time": time(FROM, TO, STEP), "vars": vars, "queries": queries, "formulas": formulas});
        if !top.is_null() {
            j["top"] = top;
        }
        j
    };
    let mut v = vec![];
    for ns in ["ns-a", "ns-empty"] {
        let mut q = mq("a", CALLS, json!([ns_m()]), json!({"op": "increase", "window": "$__range"}), json!({"op": "sum"}));
        q["fill"] = json!("zero");
        v.push((format!("R01/{ns}"), "stat: AI calls in range, 0 when none".into(), table(json!({"namespace": ns}), json!([q]), json!([])), "a",
            format!("sum(increase(margince_ai_calls_total{{kubernetes_namespace=\"{ns}\"}}[{range_s}s])) or vector(0)"), true));
    }
    for ns in ["ns-a", "ns-b", "ns-empty"] {
        let mut e = mq("e", "margince_ai_call_errors_total", json!([ns_m()]), json!({"op": "increase", "window": "$__range"}), json!({"op": "sum"}));
        e["fill"] = json!("zero");
        let mut a = mq("a", "margince_ai_call_attempts_total", json!([ns_m()]), json!({"op": "increase", "window": "$__range"}), json!({"op": "sum"}));
        a["fill"] = json!("zero");
        v.push((format!("R02/{ns}"), "stat: AI error % with clamps, 0 when none".into(), table(json!({"namespace": ns}), json!([e, a]), json!([{"name": "pct", "expr": "100 * least(e / greatest(a, 1), 1)"}])), "pct",
            format!("100 * clamp_max(sum(increase(margince_ai_call_errors_total{{kubernetes_namespace=\"{ns}\"}}[{range_s}s])) / clamp_min(sum(increase(margince_ai_call_attempts_total{{kubernetes_namespace=\"{ns}\"}}[{range_s}s])), 1), 1) or vector(0)"), true));
    }
    v.push(("R03/ns-a".into(), "stat: p95 HTTP latency over the range".into(),
        table(json!({"namespace": "ns-a"}), json!([mq("a", "margince_http_request_duration_seconds", json!([ns_m()]), json!({"op": "histogram_quantile", "q": 0.95, "window": "$__range"}), json!({"op": "sum"}))]), json!([])), "a",
        format!("histogram_quantile(0.95, sum by (le) (rate(margince_http_request_duration_seconds_bucket{{kubernetes_namespace=\"ns-a\"}}[{range_s}s])))"), true));
    for ns in ["ns-a", "ns-empty"] {
        let queries = json!([mq("a", CALLS, json!([ns_m()]), json!({"op": "rate", "window": "30m"}), json!({"op": "sum", "by": ["provider"]})), mq("z", CALLS, json!([ns_m()]), json!({"op": "last"}), json!({"op": "absent"}))]);
        let formulas = json!([{"name": "per_hour", "expr": "a * 3600"}, {"name": "zero", "expr": "z * 0"}]);
        v.push((format!("R04/{ns}"), "graph: calls per hour by provider".into(), graph(json!({"namespace": ns}), queries.clone(), formulas.clone(), J::Null), "per_hour",
            format!("sum by (provider) (rate(margince_ai_calls_total{{kubernetes_namespace=\"{ns}\"}}[30m])) * 3600"), false));
        v.push((format!("R04z/{ns}"), "graph: the panel's 0 * absent(...) line".into(), graph(json!({"namespace": ns}), queries, formulas, J::Null), "zero",
            format!("0 * absent(margince_ai_calls_total{{kubernetes_namespace=\"{ns}\"}})"), false));
    }
    let depth = "margince_job_queue_depth";
    let mut a = mq("a", depth, json!([ns_m()]), json!({"op": "last"}), json!({"op": "max", "by": ["queue", "kind", "workspace_id"]}));
    a["then"] = json!({"op": "sum", "by": ["queue", "kind"]});
    let b = mq("b", "margince_job_declared_info", json!([ns_m(), m("queue", "!=", "")]), json!({"op": "last"}), json!({"op": "sum", "by": ["queue", "kind"]}));
    let c = mq("c", depth, json!([ns_m()]), json!({"op": "max_over_time", "window": "7d"}), json!({"op": "sum", "by": ["queue", "kind"]}));
    v.push(("R05/ns-a".into(), "graph: queue depth with zero baselines (or ... * 0)".into(),
        graph(json!({"namespace": "ns-a"}), json!([a, b, c]), json!([{"name": "depth", "expr": "coalesce(a, b * 0, c * 0)", "join": "outer"}]), J::Null), "depth",
        "sum by (queue, kind) (max by (queue,kind,workspace_id) (margince_job_queue_depth{kubernetes_namespace=\"ns-a\"})) or sum by (queue, kind) (margince_job_declared_info{kubernetes_namespace=\"ns-a\",queue!=\"\"} * 0) or sum by (queue, kind) (max_over_time(margince_job_queue_depth{kubernetes_namespace=\"ns-a\"}[7d]) * 0)".into(), false));
    v.push(("R06/ns-a".into(), "graph: process uptime by app (time() - start)".into(),
        graph(json!({"namespace": "ns-a"}), json!([mq("p", "process_start_time_seconds", json!([ns_m()]), json!({"op": "last"}), json!({"op": "min", "by": ["app"]}))]), json!([{"name": "uptime", "expr": "time - p"}]), J::Null), "uptime",
        "max by (app) (time() - process_start_time_seconds{kubernetes_namespace=\"ns-a\"})".into(), false));
    let mut reqs = mq("reqs", "margince_http_request_duration_seconds_count", json!([ns_m()]), json!({"op": "rate", "window": "30m"}), json!({"op": "sum", "by": ["route"]}));
    reqs["hidden"] = json!(true);
    v.push(("R07/ns-a".into(), "graph: p95 latency of the 10 busiest routes".into(),
        graph(json!({"namespace": "ns-a"}), json!([mq("lat", "margince_http_request_duration_seconds", json!([ns_m()]), json!({"op": "histogram_quantile", "q": 0.95, "window": "30m"}), json!({"op": "sum", "by": ["route"]})), reqs]), json!([]), json!({"n": 10, "query": "reqs"})), "lat",
        "histogram_quantile(0.95, sum by (route, le) (rate(margince_http_request_duration_seconds_bucket{kubernetes_namespace=\"ns-a\"}[30m]) and on (route) topk(10, sum by (route) (rate(margince_http_request_duration_seconds_count{kubernetes_namespace=\"ns-a\"}[30m])))))".into(), false));
    for ns in ["ns-a", "ns-empty"] {
        let mut q = mq("a", CALLS, json!([ns_m()]), json!({"op": "increase", "window": "$__range"}), json!({"op": "sum", "by": ["provider", "model"]}));
        q["having"] = json!({"op": ">", "value": 0.0});
        q["then"] = json!({"op": "count"});
        q["fill"] = json!("zero");
        v.push((format!("R09/{ns}"), "stat: number of active models".into(), table(json!({"namespace": ns}), json!([q]), json!([])), "a",
            format!("count(count by (provider,model) (increase(margince_ai_calls_total{{kubernetes_namespace=\"{ns}\"}}[{range_s}s]) > 0)) or vector(0)"), true));
    }
    let mut cpu = mq("cpu", "container_cpu_usage_seconds_total", json!([m("namespace", "=", "$namespace"), m("container", "!=", ""), m("container", "!=", "POD")]), json!({"op": "rate", "window": "30m"}), json!({"op": "sum", "by": ["pod", "role"]}));
    cpu["derive"] = json!([{"label": "role", "from": "pod", "regex": "([a-z]+)-application-.*", "replacement": "$1"}, {"label": "role", "from": "pod", "regex": "([a-z]+)(?:-master)?-[0-9]+", "replacement": "$1"}]);
    v.push(("R10/ns-a".into(), "graph: CPU by pod with role derived from the pod name".into(), graph(json!({"namespace": "ns-a"}), json!([cpu]), json!([]), J::Null), "cpu",
        "label_replace(label_replace(sum by (pod) (rate(container_cpu_usage_seconds_total{namespace=\"ns-a\",container!=\"\",container!=\"POD\"}[30m])), \"role\", \"$1\", \"pod\", \"([a-z]+)-application-.*\"), \"role\", \"$1\", \"pod\", \"([a-z]+)(?:-master)?-[0-9]+\")".into(), false));
    v.push(("R11/ns-a".into(), "stat: declared job kinds".into(), table(json!({"namespace": "ns-a"}), json!([mq("a", "margince_job_declared_info", json!([ns_m()]), json!({"op": "last"}), json!({"op": "count"}))]), json!([])), "a",
        "count(margince_job_declared_info{kubernetes_namespace=\"ns-a\"})".into(), true));
    v
}

fn run_dashboard_cases(conn: &Connection, prom: &Prom, out: &mut Vec<CaseResult>) -> Result<()> {
    for (id, function, tqs_json, output, promql, instant) in dashboard_cases() {
        let theirs = if instant { prom.instant(&promql, TO)? } else { prom.range(&promql, FROM, TO, STEP)? };
        let (mine, ms) = ours(conn, &tqs_json, output).with_context(|| id.clone())?;
        let d = diff(&theirs, &mine);
        let empty_both = theirs.is_empty() && mine.is_empty();
        out.push(CaseResult {
            id: id.clone(),
            area: "real dashboard panels vs Prometheus",
            function,
            reference: format!("`{}`", promql.replace('`', "'")),
            pass: d.missing.is_empty() && d.extra.is_empty() && d.mismatched == 0 && d.prom_nonfinite == 0 && (d.compared > 0 || empty_both),
            detail: format!("{} points equal (max rel. diff {:.1e}); missing {}, extra {}, mismatched {}{}{}", d.compared, d.max_rel, d.missing.len(), d.extra.len(), d.mismatched,
                if empty_both { "; both empty, as expected" } else { "" },
                if d.missing.is_empty() && d.extra.is_empty() { String::new() } else { format!(" e.g. {:?}", d.missing.first().or(d.extra.first())) }),
            ms,
        });
    }
    Ok(())
}

fn ours(conn: &Connection, tqs_json: &J, output: &str) -> Result<(BTreeMap<(String, i64), f64>, f64)> {
    let t: Tqs = Tqs::parse(&tqs_json.to_string()).map_err(|p| anyhow!("{p:?}"))?;
    let r = t.validate(TO).map_err(|p| anyhow!("validation: {p:?}"))?;
    let start = Instant::now();
    let rows = compile::run(conn, &t, &r)?;
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    Ok((rows.into_iter().filter(|r| r.output == output).map(|r| ((r.key, r.t), r.value)).collect(), ms))
}

struct Diff {
    compared: usize,
    missing: Vec<(String, i64)>,
    extra: Vec<(String, i64)>,
    mismatched: usize,
    max_rel: f64,
    prom_nonfinite: usize,
}

fn diff(theirs: &Points, mine: &BTreeMap<(String, i64), f64>) -> Diff {
    let mut d = Diff { compared: 0, missing: vec![], extra: vec![], mismatched: 0, max_rel: 0.0, prom_nonfinite: 0 };
    for (k, pv) in theirs {
        if !pv.is_finite() {
            d.prom_nonfinite += 1;
            if mine.contains_key(k) {
                d.extra.push(k.clone());
            }
            continue;
        }
        match mine.get(k) {
            None => d.missing.push(k.clone()),
            Some(mv) => {
                d.compared += 1;
                let rel = if pv.abs() < 1e-12 { (mv - pv).abs() } else { ((mv - pv) / pv).abs() };
                d.max_rel = d.max_rel.max(rel);
                if rel > REL_TOL {
                    d.mismatched += 1;
                }
            }
        }
    }
    d.extra.extend(mine.keys().filter(|k| !theirs.contains_key(*k)).cloned());
    d
}

fn run_metric_cases(conn: &Connection, prom: &Prom, out: &mut Vec<CaseResult>) -> Result<()> {
    for c in metric_cases() {
        let theirs = if c.instant { prom.instant(&c.promql, TO)? } else { prom.range(&c.promql, FROM, TO, STEP)? };
        let (mine, ms) = ours(conn, &c.tqs, c.output).with_context(|| c.id.to_string())?;
        let d = diff(&theirs, &mine);
        let pass = d.missing.is_empty() && d.extra.is_empty() && d.mismatched == 0 && d.prom_nonfinite == 0 && d.compared > 0 || (c.id == "M31" && d.compared == 0 && d.missing.is_empty() && d.extra.is_empty());
        out.push(CaseResult {
            id: c.id.into(),
            area: "metrics vs Prometheus",
            function: c.function.into(),
            reference: format!("`{}`", c.promql),
            pass,
            detail: format!(
                "{} points equal (max rel. diff {:.1e}); missing {}, extra {}, mismatched {}{}",
                d.compared,
                d.max_rel,
                d.missing.len(),
                d.extra.len(),
                d.mismatched,
                if d.missing.is_empty() && d.extra.is_empty() { String::new() } else { format!(" e.g. {:?}", d.missing.first().or(d.extra.first())) }
            ),
            ms,
        });
    }
    Ok(())
}

fn run_divergences(conn: &Connection, prom: &Prom, out: &mut Vec<CaseResult>) -> Result<()> {
    let rate5 = json!({"op": "rate", "window": "5m"});
    // D1: no error series at all for openai/classify. PromQL drops the group; TQS fill zero keeps it at 0.
    let promql = "sum by (provider) (rate(margince_ai_calls_total{task=\"classify\",outcome=\"error\"}[5m])) / sum by (provider) (rate(margince_ai_calls_total{task=\"classify\"}[5m]))";
    let mut errq = mq("err", CALLS, json!([m("task", "=", "classify"), m("outcome", "=", "error")]), rate5.clone(), json!({"op": "sum", "by": ["provider"]}));
    errq["fill"] = json!("zero");
    let t = ts(json!([errq, mq("all", CALLS, json!([m("task", "=", "classify")]), rate5.clone(), json!({"op": "sum", "by": ["provider"]}))]), json!([{"name": "ratio", "expr": "err / all"}]));
    let theirs = prom.range(promql, FROM, TO, STEP)?;
    let (mine, ms) = ours(conn, &t, "ratio")?;
    let prom_openai = theirs.keys().filter(|(k, _)| k == "provider=openai").count();
    let ours_openai: Vec<f64> = mine.iter().filter(|((k, _), _)| k == "provider=openai").map(|(_, v)| *v).collect();
    let common_equal = theirs.iter().filter(|(k, v)| v.is_finite() && mine.get(*k).is_some_and(|m| (m - **v).abs() <= 1e-9 * v.abs().max(1e-12))).count();
    let prom_finite = theirs.values().filter(|v| v.is_finite()).count();
    out.push(CaseResult {
        id: "D1".into(),
        area: "deliberate divergence",
        function: "fill: zero on a missing operand".into(),
        reference: format!("`{promql}`"),
        pass: prom_openai == 0 && !ours_openai.is_empty() && ours_openai.iter().all(|v| *v == 0.0) && common_equal == prom_finite,
        detail: format!("Prometheus: no openai points (no error series). TQS: {} openai points, all 0. Other {common_equal}/{prom_finite} points equal.", ours_openai.len()),
        ms,
    });
    // D2: 0/0 — anthropic/classify errors are a flat zero counter.
    let promql = "sum by (provider) (rate(margince_ai_calls_total{task=\"classify\",outcome=\"error\",provider=\"anthropic\"}[5m])) / sum by (provider) (rate(margince_ai_calls_total{task=\"classify\",outcome=\"error\",provider=\"anthropic\"}[5m]))";
    let q = |n: &str| mq(n, CALLS, json!([m("task", "=", "classify"), m("outcome", "=", "error"), m("provider", "=", "anthropic")]), rate5.clone(), json!({"op": "sum", "by": ["provider"]}));
    let t = ts(json!([q("a"), q("b")]), json!([{"name": "r", "expr": "a / b"}]));
    let theirs = prom.range(promql, FROM, TO, STEP)?;
    let (mine, ms) = ours(conn, &t, "r")?;
    let nan = theirs.values().filter(|v| v.is_nan()).count();
    out.push(CaseResult {
        id: "D2".into(),
        area: "deliberate divergence",
        function: "division by zero".into(),
        reference: format!("`{promql}`"),
        pass: nan == theirs.len() && nan > 0 && mine.is_empty(),
        detail: format!("Prometheus: {nan} NaN points. TQS: {} points (x/0 is no value, rendered as a gap).", mine.len()),
        ms,
    });
    Ok(())
}

// ---------- spans / logs / traces vs an independent Rust computation ----------

fn r7(mut xs: Vec<f64>, q: f64) -> f64 {
    xs.sort_by(f64::total_cmp);
    let h = (xs.len() - 1) as f64 * q;
    let lo = h.floor() as usize;
    if lo + 1 >= xs.len() { xs[lo] } else { xs[lo] + (h - lo as f64) * (xs[lo + 1] - xs[lo]) }
}

fn start_ms(s: &Span) -> i64 {
    s.start_ns / 1_000_000
}

type Series = BTreeMap<(String, i64), f64>;

fn bucketed<T>(items: &[T], ts: impl Fn(&T) -> i64, key: impl Fn(&T) -> String, step: i64, from: i64, to: i64, value: impl Fn(&[&T]) -> Option<f64>) -> Series {
    let mut groups: BTreeMap<(String, i64), Vec<&T>> = BTreeMap::new();
    for it in items {
        let t = ts(it);
        if t <= from - step || t > to {
            continue;
        }
        // The point at grid t covers (t - step, t]: the first grid point >= t.
        let k = (t - from + step - 1).div_euclid(step);
        let g = from + k * step;
        if g <= to {
            groups.entry((key(it), g)).or_default().push(it);
        }
    }
    groups.into_iter().filter_map(|(k, v)| value(&v).map(|x| (k, x))).collect()
}

fn compare(id: &str, area: &'static str, function: &str, reference: &str, expected: &Series, got: &Series, ms: f64) -> CaseResult {
    let missing = expected.keys().filter(|k| !got.contains_key(*k)).count();
    let extra = got.keys().filter(|k| !expected.contains_key(*k)).count();
    let mut worst: f64 = 0.0;
    for (k, e) in expected {
        if let Some(g) = got.get(k) {
            worst = worst.max(if e.abs() < 1e-12 { (g - e).abs() } else { ((g - e) / e).abs() });
        }
    }
    CaseResult {
        id: id.into(),
        area,
        function: function.into(),
        reference: reference.into(),
        pass: missing == 0 && extra == 0 && worst <= REL_TOL && !expected.is_empty(),
        detail: format!("{} points; missing {missing}, extra {extra}, max rel. diff {worst:.1e}", expected.len()),
        ms,
    }
}

fn run_event_cases(conn: &Connection, spans: &[Span], logs: &[Log], orphan: [u8; 16], out: &mut Vec<CaseResult>) -> Result<()> {
    let step = 600_000;
    let tsj = |queries: J, formulas: J, step: i64| json!({"tqs": "3", "id": "case", "kind": "timeseries", "time": time(FROM, TO, step), "queries": queries, "formulas": formulas});
    let run = |j: &J, output: &str| ours(conn, j, output);
    let svc = |s: &Span| format!("service_name={}", s.service);

    // S1 span rate by service.
    let exp = bucketed(spans, start_ms, svc, STEP, FROM, TO, |v| Some(v.len() as f64 / 60.0));
    let (got, ms) = run(&tsj(json!([{"name": "a", "spans": [], "measure": {"op": "rate"}, "agg": {"by": ["service_name"]}}]), json!([]), STEP), "a")?;
    out.push(compare("S1", "spans vs Rust", "span rate by service", "count / step, by start time", &exp, &got, ms));
    // S2 error count by service.
    let errs: Vec<Span> = spans.iter().filter(|s| s.status == 2).cloned().collect();
    let exp = bucketed(&errs, start_ms, svc, STEP, FROM, TO, |v| Some(v.len() as f64));
    let (got, ms) = run(&tsj(json!([{"name": "a", "spans": [{"field": "status_code", "op": "=", "value": 2}], "measure": {"op": "count"}, "agg": {"by": ["service_name"]}}]), json!([]), STEP), "a")?;
    out.push(compare("S2", "spans vs Rust", "error count by service", "count where status=error", &exp, &got, ms));
    // S3 p95 duration of internal payment spans, 10 min steps.
    let pay: Vec<Span> = spans.iter().filter(|s| s.service == "payment" && fixtures::span_kind(s) == 1).cloned().collect();
    let exp = bucketed(&pay, start_ms, |_| String::new(), step, FROM, TO, |v| Some(r7(v.iter().map(|s| s.duration_ns as f64 / 1e6).collect(), 0.95)));
    let (got, ms) = run(&tsj(json!([{"name": "a", "spans": [{"field": "service_name", "op": "=", "value": "payment"}, {"field": "span_kind", "op": "=", "value": 1}], "measure": {"op": "quantile", "field": "duration_ms", "q": 0.95}}]), json!([]), step), "a")?;
    out.push(compare("S3", "spans vs Rust", "p95 duration (exact, ad hoc)", "R-7 quantile", &exp, &got, ms));
    // S4 error % by service with fill zero.
    let all = bucketed(spans, start_ms, svc, step, FROM, TO, |v| Some(v.len() as f64));
    let err = bucketed(&errs, start_ms, svc, step, FROM, TO, |v| Some(v.len() as f64));
    let exp: Series = all.iter().map(|(k, a)| (k.clone(), err.get(k).copied().unwrap_or(0.0) / a * 100.0)).collect();
    let (got, ms) = run(
        &tsj(
            json!([{"name": "e", "spans": [{"field": "status_code", "op": "=", "value": 2}], "measure": {"op": "count"}, "agg": {"by": ["service_name"]}, "fill": "zero"},
                   {"name": "a", "spans": [], "measure": {"op": "count"}, "agg": {"by": ["service_name"]}}]),
            json!([{"name": "pct", "expr": "e / a * 100"}]),
            step,
        ),
        "pct",
    )?;
    out.push(compare("S4", "spans vs Rust", "error % by service (fill zero)", "errors/all*100, 0 when no errors", &exp, &got, ms));
    // S5 table: refund route over 50 ms, by service, over the whole range.
    let mut exp = Series::new();
    for s in spans.iter().filter(|s| s.http_route == "/api/refund" && s.duration_ns as f64 / 1e6 > 50.0 && start_ms(s) > FROM && start_ms(s) <= TO) {
        *exp.entry((svc(s), TO)).or_insert(0.0) += 1.0;
    }
    let (got, ms) = run(
        &json!({"tqs": "3", "id": "case", "kind": "table", "time": {"from": FROM, "to": TO},
                "queries": [{"name": "a", "spans": [{"field": "attr.http.route", "op": "=", "value": "/api/refund"}, {"field": "duration_ms", "op": ">", "value": 50}], "measure": {"op": "count"}, "agg": {"by": ["service_name"]}}]}),
        "a",
    )?;
    out.push(compare("S5", "spans vs Rust", "attribute + duration filter, table", "count over (from, to]", &exp, &got, ms));
    // S6 regex on span name, avg duration.
    let db: Vec<Span> = spans.iter().filter(|s| s.name.starts_with("SELECT")).cloned().collect();
    let exp = bucketed(&db, start_ms, |_| String::new(), step, FROM, TO, |v| Some(v.iter().map(|s| s.duration_ns as f64 / 1e6).sum::<f64>() / v.len() as f64));
    let (got, ms) = run(&tsj(json!([{"name": "a", "spans": [{"field": "span_name", "op": "=~", "value": "SELECT.*"}], "measure": {"op": "avg", "field": "duration_ms"}}]), json!([]), step), "a")?;
    out.push(compare("S6", "spans vs Rust", "regex on span name, avg duration", "mean", &exp, &got, ms));

    // Logs.
    let lms = |l: &Log| l.ts_ns / 1_000_000;
    let exp = bucketed(logs, lms, |l| format!("severity_text={}", fixtures::severity_text(l.severity)), step, FROM, TO, |v| Some(v.len() as f64));
    let (got, ms) = run(&tsj(json!([{"name": "a", "logs": [], "measure": {"op": "count"}, "agg": {"by": ["severity_text"]}}]), json!([]), step), "a")?;
    out.push(compare("L1", "logs vs Rust", "log count by severity", "count", &exp, &got, ms));
    let mut exp = Series::new();
    for l in logs.iter().filter(|l| l.body.contains("timed out") && lms(l) > FROM && lms(l) <= TO) {
        *exp.entry((format!("service_name={}", l.service), TO)).or_insert(0.0) += 1.0;
    }
    let (got, ms) = run(
        &json!({"tqs": "3", "id": "case", "kind": "table", "time": {"from": FROM, "to": TO}, "queries": [{"name": "a", "logs": [{"field": "body", "op": "contains", "value": "timed out"}], "measure": {"op": "count"}, "agg": {"by": ["service_name"]}}]}),
        "a",
    )?;
    out.push(compare("L2", "logs vs Rust", "substring search count, table", "count", &exp, &got, ms));

    // Lists.
    let list = |j: J| -> Result<(Vec<Vec<(String, String)>>, f64)> {
        let t = Tqs::parse(&j.to_string()).map_err(|p| anyhow!("{p:?}"))?;
        let r = t.validate(TO).map_err(|p| anyhow!("validation: {p:?}"))?;
        let s = Instant::now();
        let rows = compile::run_list(conn, &t, &r)?;
        Ok((rows, s.elapsed().as_secs_f64() * 1000.0))
    };
    let cell = |row: &Vec<(String, String)>, name: &str| row.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone()).unwrap_or_default();
    let hexid = |id: &[u8; 16]| id.iter().map(|b| format!("{b:02x}")).collect::<String>();

    // L3 log search, newest first.
    let mut exp: Vec<&Log> = logs.iter().filter(|l| l.body.contains("declined") && lms(l) > FROM && lms(l) <= TO).collect();
    exp.sort_by(|a, b| b.ts_ns.cmp(&a.ts_ns).then(a.body.cmp(&b.body)));
    let exp: Vec<String> = exp.iter().take(20).map(|l| l.body.clone()).collect();
    let (rows, ms) = list(json!({"tqs": "3", "id": "case", "kind": "log_search", "time": {"from": FROM, "to": TO}, "search": {"logs": [{"field": "body", "op": "=~", "value": ".*declined.*"}]}, "limit": 20}))?;
    let got: Vec<String> = rows.iter().map(|r| cell(r, "body")).collect();
    out.push(CaseResult { id: "L3".into(), area: "logs vs Rust", function: "log search (regex, newest first, limit)".into(), reference: "filter + sort + take 20".into(), pass: got == exp && !exp.is_empty(), detail: format!("{} rows, order identical: {}", got.len(), got == exp), ms });
    // L4 patterns.
    let mut counts: HashMap<u64, usize> = HashMap::new();
    for l in logs.iter().filter(|l| lms(l) > FROM && lms(l) <= TO) {
        *counts.entry(fixtures::pattern_hash(&l.body)).or_default() += 1;
    }
    let mut exp: Vec<(u64, usize)> = counts.into_iter().collect();
    exp.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.to_string().cmp(&b.0.to_string())));
    let (rows, ms) = list(json!({"tqs": "3", "id": "case", "kind": "log_patterns", "time": {"from": FROM, "to": TO}, "search": {"logs": []}, "limit": 10}))?;
    let got: Vec<(u64, usize)> = rows.iter().map(|r| (cell(r, "pattern_hash").parse().unwrap_or(0), cell(r, "n").parse().unwrap_or(0))).collect();
    let examples: Vec<String> = rows.iter().map(|r| fixtures::mask_body(&cell(r, "example"))).collect();
    out.push(CaseResult {
        id: "L4".into(),
        area: "logs vs Rust",
        function: "log patterns (deterministic masking)".into(),
        reference: "group by masked-body hash".into(),
        pass: got == exp && exp.len() == 5,
        detail: format!("{} patterns (5 templates planted); counts identical: {}; templates: {}", got.len(), got == exp, examples.join(" | ")),
        ms,
    });
    // L5 logs of one trace.
    let probe = spans.iter().find(|s| s.service == "payment" && s.status == 2 && start_ms(s) > FROM && start_ms(s) <= TO).ok_or_else(|| anyhow!("no failed payment span in range"))?.trace_id;
    let exp: usize = logs.iter().filter(|l| l.trace_id == Some(probe) && lms(l) > FROM && lms(l) <= TO).count();
    let (rows, ms) = list(json!({"tqs": "3", "id": "case", "kind": "log_search", "time": {"from": FROM, "to": TO}, "search": {"logs": [{"field": "trace_id", "op": "=", "value": hexid(&probe)}]}}))?;
    out.push(CaseResult { id: "L5".into(), area: "logs vs Rust", function: "logs of one trace".into(), reference: "filter on trace id".into(), pass: rows.len() == exp && exp > 0 && rows.iter().all(|r| cell(r, "trace_id") == hexid(&probe)), detail: format!("{} of {exp} log lines", rows.len()), ms });

    // T1 trace search.
    let mut traces: BTreeMap<[u8; 16], Vec<&Span>> = BTreeMap::new();
    for s in spans.iter().filter(|s| start_ms(s) > FROM && start_ms(s) <= TO) {
        traces.entry(s.trace_id).or_default().push(s);
    }
    let mut exp: Vec<(String, f64)> = traces
        .iter()
        .filter(|(_, v)| v.iter().any(|s| s.service == "payment" && s.status == 2))
        .map(|(id, v)| {
            let start = v.iter().map(|s| s.start_ns).min().unwrap_or(0);
            let end = v.iter().map(|s| s.start_ns + s.duration_ns as i64).max().unwrap_or(0);
            (hexid(id), (end - start) as f64 / 1e6)
        })
        .filter(|(_, d)| *d > 1000.0)
        .collect();
    exp.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    exp.truncate(10);
    let (rows, ms) = list(json!({"tqs": "3", "id": "case", "kind": "trace_search", "time": {"from": FROM, "to": TO},
        "search": {"spans": [{"field": "service_name", "op": "=", "value": "payment"}, {"field": "status_code", "op": "=", "value": 2}], "trace": [{"field": "duration_ms", "op": ">", "value": 1000}], "order": "duration_desc"}, "limit": 10}))?;
    let got: Vec<String> = rows.iter().map(|r| cell(r, "trace_id")).collect();
    let exp_ids: Vec<String> = exp.iter().map(|e| e.0.clone()).collect();
    out.push(CaseResult { id: "T1".into(), area: "traces vs Rust", function: "trace search: failed payment, > 1 s, slowest first".into(), reference: "group spans by trace".into(), pass: got == exp_ids && !exp_ids.is_empty(), detail: format!("{} traces, order identical: {}", got.len(), got == exp_ids), ms });
    // T2 waterfall of a complete cross-region trace.
    let full = traces.iter().find(|(_, v)| v.len() == 5).map(|(k, _)| *k).ok_or_else(|| anyhow!("no complete trace"))?;
    let (rows, ms) = list(json!({"tqs": "3", "id": "case", "kind": "trace", "trace_id": hexid(&full)}))?;
    let shape: Vec<String> = rows.iter().map(|r| format!("{}:{}:{}", cell(r, "depth"), cell(r, "region"), cell(r, "service_name"))).collect();
    let want = ["0:us-east:gateway", "1:us-east:checkout", "2:us-east:checkout", "2:eu-west:payment", "3:eu-west:ledger"];
    out.push(CaseResult { id: "T2".into(), area: "traces vs Rust", function: "waterfall across regions".into(), reference: "planted parent/child tree".into(), pass: shape == want, detail: shape.join(" → "), ms });
    // T3 waterfall with a lost parent span.
    let (rows, ms) = list(json!({"tqs": "3", "id": "case", "kind": "trace", "trace_id": hexid(&orphan)}))?;
    let orphans = rows.iter().filter(|r| cell(r, "orphan") == "true").count();
    out.push(CaseResult { id: "T3".into(), area: "traces vs Rust", function: "waterfall with a lost parent span".into(), reference: "checkout span removed".into(), pass: rows.len() == 4 && orphans == 2, detail: format!("{} spans shown, {orphans} flagged orphan (expected 4 and 2)", rows.len()), ms });
    Ok(())
}

// ---------- validation and safety ----------

fn base() -> J {
    ts(json!([mq("a", CALLS, json!([]), json!({"op": "rate", "window": "5m"}), json!({"op": "sum", "by": ["provider"]}))]), json!([]))
}

fn with(f: impl Fn(&mut J)) -> J {
    let mut j = base();
    f(&mut j);
    j
}

fn validation_cases() -> Vec<(&'static str, &'static str, J)> {
    vec![
        ("V01", "schema", with(|j| j["promql"] = json!("rate(x[5m])"))),
        ("V02", "version", with(|j| j["tqs"] = json!("2"))),
        ("V03", "time", with(|j| { j.as_object_mut().map(|o| o.remove("time")); })),
        ("V04", "time_order", with(|j| j["time"] = json!({"from": TO, "to": FROM, "step": "1m"}))),
        ("V05", "too_many_points", with(|j| j["time"] = json!({"from": "-2d", "to": "now", "step": "1s"}))),
        ("V06", "step", with(|j| j["time"]["step"] = json!("5x"))),
        ("V07", "queries", with(|j| j["queries"] = json!([]))),
        ("V08", "queries", with(|j| j["queries"] = json!((0..11).map(|i| mq(&format!("q{i}"), CALLS, json!([]), json!({"op": "last"}), J::Null)).collect::<Vec<_>>()))),
        ("V09", "name", with(|j| j["queries"] = json!([mq("a", CALLS, json!([]), json!({"op": "last"}), J::Null), mq("a", CALLS, json!([]), json!({"op": "last"}), J::Null)]))),
        ("V10", "source", with(|j| j["queries"][0]["spans"] = json!([]))),
        ("V11", "fn", with(|j| { j["queries"][0].as_object_mut().map(|o| o.remove("fn")); })),
        ("V12", "window", with(|j| j["queries"][0]["fn"] = json!({"op": "rate"}))),
        ("V13", "window", with(|j| j["queries"][0]["fn"] = json!({"op": "last", "window": "5m"}))),
        ("V14", "q", with(|j| j["queries"][0]["fn"] = json!({"op": "histogram_quantile", "window": "5m", "q": 1.5}))),
        ("V15", "q", with(|j| j["queries"][0]["fn"] = json!({"op": "rate", "window": "5m", "q": 0.5}))),
        ("V16", "label", with(|j| j["queries"][0]["match"] = json!([m("provider'] OR 1=1 --", "=", "x")]))),
        ("V17", "op", with(|j| j["queries"][0]["match"] = json!([m("provider", "~=", "x")]))),
        ("V18", "regex", with(|j| j["queries"][0]["match"] = json!([m("provider", "=~", "(")]))),
        ("V19", "regex", with(|j| j["queries"][0]["match"] = json!([m("provider", "=~", &"a".repeat(300))]))),
        ("V20", "formula_ref", with(|j| j["formulas"] = json!([{"name": "f", "expr": "a / missing"}]))),
        ("V21", "formula", with(|j| j["formulas"] = json!([{"name": "f", "expr": "sqrt(a)"}]))),
        ("V22", "formula", with(|j| j["formulas"] = json!([{"name": "f", "expr": "a; DROP TABLE metrics_points"}]))),
        ("V23", "formula", with(|j| j["formulas"] = json!([{"name": "f", "expr": "1 + 2"}]))),
        ("V24", "formula_join", with(|j| {
            j["queries"] = json!([mq("a", CALLS, json!([]), json!({"op": "rate", "window": "5m"}), json!({"op": "sum", "by": ["provider"]})), mq("b", CALLS, json!([]), json!({"op": "rate", "window": "5m"}), json!({"op": "sum", "by": ["task"]}))]);
            j["formulas"] = json!([{"name": "f", "expr": "a / b"}]);
        })),
        ("V25", "agg", with(|j| j["queries"][0]["agg"] = json!({"op": "sum", "by": ["a"], "without": ["b"]}))),
        ("V26", "agg", with(|j| j["queries"][0]["agg"] = json!({"by": ["provider"]}))),
        ("V27", "field", with(|j| j["queries"] = json!([{"name": "s", "spans": [{"field": "user.email", "op": "=", "value": "x"}], "measure": {"op": "count"}}]))),
        ("V28", "cond", with(|j| j["queries"] = json!([{"name": "s", "spans": [{"field": "service_name", "op": ">", "value": 5}], "measure": {"op": "count"}}]))),
        ("V29", "q", with(|j| j["queries"] = json!([{"name": "s", "spans": [], "measure": {"op": "quantile", "field": "duration_ms"}}]))),
        ("V30", "measure", with(|j| j["queries"] = json!([{"name": "s", "logs": [], "measure": {"op": "avg", "field": "duration_ms"}}]))),
        ("V31", "trace_id", json!({"tqs": "3", "id": "t", "kind": "trace", "trace_id": "abc"})),
        ("V32", "offset", with(|j| j["queries"][0]["offset"] = json!("-5m"))),
        ("V33", "having", with(|j| j["queries"][0]["having"] = json!({"op": "LIKE", "value": 1.0}))),
        ("V34", "top", with(|j| j["top"] = json!({"n": 0}))),
        ("V35", "schema", with(|j| j["queries"][0]["fn"] = json!({"op": "predict_linear", "window": "1h"}))),
        ("V36", "metric", with(|j| j["queries"][0]["metric"] = json!("rate(x) OR 1"))),
    ]
}

fn run_validation(conn: &Connection, out: &mut Vec<CaseResult>) -> Result<()> {
    for (id, code, j) in validation_cases() {
        let start = Instant::now();
        let got: Vec<&'static str> = match Tqs::parse(&j.to_string()) {
            Err(p) => vec![p.code],
            Ok(t) => match t.validate(TO) {
                Ok(_) => vec![],
                Err(ps) => ps.iter().map(|p| p.code).collect(),
            },
        };
        out.push(CaseResult {
            id: id.into(),
            area: "validation",
            function: format!("refuse with `{code}`"),
            reference: "—".into(),
            pass: got.contains(&code),
            detail: format!("problem codes: {got:?}"),
            ms: start.elapsed().as_secs_f64() * 1000.0,
        });
    }
    // Values that look like SQL must be data: accepted, bound, harmless.
    let inj = with(|j| j["queries"][0]["match"] = json!([m("provider", "=", "x' OR '1'='1")]));
    let (rows, ms) = ours(conn, &inj, "a")?;
    out.push(CaseResult { id: "A1".into(), area: "validation", function: "SQL-looking matcher value is bound, not executed".into(), reference: "—".into(), pass: rows.is_empty(), detail: format!("{} rows (expected 0)", rows.len()), ms });
    let inj = json!({"tqs": "3", "id": "case", "kind": "log_search", "time": {"from": FROM, "to": TO}, "search": {"logs": [{"field": "body", "op": "contains", "value": "'; DROP TABLE logs; --"}]}});
    let t = Tqs::parse(&inj.to_string()).map_err(|p| anyhow!("{p:?}"))?;
    let r = t.validate(TO).map_err(|p| anyhow!("{p:?}"))?;
    let rows = compile::run_list(conn, &t, &r)?;
    let logs_left: i64 = conn.query_row("SELECT count(*) FROM logs", [], |r| r.get(0))?;
    out.push(CaseResult { id: "A2".into(), area: "validation", function: "SQL-looking search text is bound, not executed".into(), reference: "—".into(), pass: rows.is_empty() && logs_left > 0, detail: format!("{} rows; logs table still has {logs_left} rows", rows.len()), ms: 0.0 });
    Ok(())
}

/// The same compiled queries on DuckLake tables in MinIO must give the in-memory results.
pub fn run_on_lake(env: &crate::lake::Env, work: &std::path::Path) -> Result<Vec<CaseResult>> {
    std::fs::create_dir_all(work)?;
    let mem = Connection::open_in_memory()?;
    let series = fixtures::metrics();
    fixtures::load_metrics(&mem, &series, work)?;
    let (mut spans, _) = fixtures::spans();
    fixtures::load_spans(&mem, &mut spans, work)?;
    let logs = fixtures::logs(&spans);
    fixtures::load_logs(&mem, &logs, work)?;
    let lake = env.lake()?;
    lake.execute_batch("CREATE SCHEMA IF NOT EXISTS tqs; USE lake.tqs;")?;
    for t in ["metrics_series", "metrics_points", "spans", "logs"] {
        // DuckLake writes each table into MinIO from a Parquet export of the in-memory one.
        let file = work.join(format!("{t}.export.parquet"));
        mem.execute_batch(&format!("COPY {t} TO '{}' (FORMAT parquet)", file.display()))?;
        lake.execute_batch(&format!("DROP TABLE IF EXISTS {t}; CREATE TABLE {t} AS SELECT * FROM read_parquet('{}');", file.display()))?;
    }
    let mut out = vec![];
    let cases = metric_cases();
    for id in ["M02", "M05", "M06", "M10", "M19", "M22"] {
        let c = cases.iter().find(|c| c.id == id).ok_or_else(|| anyhow!("case {id}"))?;
        let (a, ms_mem) = ours(&mem, &c.tqs, c.output)?;
        let (b, ms_lake) = ours(&lake, &c.tqs, c.output)?;
        let equal = a.len() == b.len() && a.iter().all(|(k, v)| b.get(k).is_some_and(|w| (w - v).abs() <= 1e-9 * v.abs().max(1e-12)));
        out.push(CaseResult { id: format!("{id}-lake"), area: "same queries on DuckLake + MinIO", function: c.function.into(), reference: "in-memory result".into(), pass: equal && !a.is_empty(), detail: format!("{} points identical: {equal}; memory {ms_mem:.1} ms, lake {ms_lake:.1} ms", a.len()), ms: ms_lake });
    }
    Ok(out)
}

pub fn run_all(tools: &std::path::Path, work: &std::path::Path) -> Result<Vec<CaseResult>> {
    std::fs::create_dir_all(work)?;
    let conn = Connection::open_in_memory()?;
    let mut series = fixtures::metrics();
    series.extend(fixtures::dashboard_metrics());
    fixtures::load_metrics(&conn, &series, work)?;
    let (mut spans, orphan) = fixtures::spans();
    fixtures::load_spans(&conn, &mut spans, work)?;
    let logs = fixtures::logs(&spans);
    fixtures::load_logs(&conn, &logs, work)?;
    let prom = Prom::start(tools, &work.join("prom"), &fixtures::openmetrics(&series))?;
    let version = std::process::Command::new("go").args(["version", "-m"]).arg(tools.join("prometheus")).output().ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.lines().find(|l| l.trim_start().starts_with("mod\t")).map(|l| l.split('\t').nth(3).unwrap_or("").to_string()))
        .map(|v| format!("module {v} (= Prometheus 3.{})", v.trim_start_matches("v0.").split('.').next().unwrap_or("?").trim_start_matches('3')))
        .unwrap_or_else(|| prom.version.clone());
    println!("fixtures: {} series / {} samples, {} spans, {} logs; reference Prometheus {}", series.len(), series.iter().map(|s| s.points.len()).sum::<usize>(), spans.len(), logs.len(), version);
    let semantic = |out: &mut Vec<CaseResult>| -> Result<()> {
        run_metric_cases(&conn, &prom, out)?;
        run_divergences(&conn, &prom, out)?;
        run_dashboard_cases(&conn, &prom, out)?;
        run_event_cases(&conn, &spans, &logs, orphan, out)
    };
    let mut out = vec![];
    semantic(&mut out)?;
    run_validation(&conn, &mut out)?;
    // Mutation check: each mutant breaks one rule; the suite must catch every one.
    let mut caught: Vec<(&str, &str, Vec<String>)> = vec![];
    for (name, what) in tqs::MUTANTS {
        *tqs::MUTANT.write().map_err(|_| anyhow!("mutant lock poisoned"))? = name.to_string();
        let mut m = vec![];
        let res = semantic(&mut m);
        *tqs::MUTANT.write().map_err(|_| anyhow!("mutant lock poisoned"))? = String::new();
        let mut red: Vec<String> = m.iter().filter(|c| !c.pass).map(|c| c.id.clone()).collect();
        if let Err(e) = res {
            red.push(format!("error: {}", e.to_string().lines().next().unwrap_or_default()));
        }
        println!("mutant {name:<20} caught by {} case(s): {}", red.len(), red.join(" "));
        caught.push((name, what, red));
    }
    let mut md = String::new();
    let passed = out.iter().filter(|c| c.pass).count();
    writeln!(md, "# TQS 3 coverage results\n")?;
    writeln!(md, "Reference: Prometheus {} on the same fixtures. Engine: DuckDB {}. Fixtures: {} series / {} samples (Margince-shaped), {} spans, {} logs.\n",
        version, conn.query_row::<String, _, _>("SELECT version()", [], |r| r.get(0))?, series.len(), series.iter().map(|s| s.points.len()).sum::<usize>(), spans.len(), logs.len())?;
    writeln!(md, "**{passed}/{} cases pass.** Range {FROM}..{TO} ms, step {STEP} ms unless stated; equality tolerance {REL_TOL:e} relative.\n", out.len())?;
    let mut area = "";
    for c in &out {
        if c.area != area {
            area = c.area;
            writeln!(md, "\n## {area}\n\n| ID | Function | Reference | Result | Detail | ms |\n| :--- | :--- | :--- | :--- | :--- | ---: |")?;
        }
        writeln!(md, "| {} | {} | {} | {} | {} | {:.1} |", c.id, c.function, c.reference.replace('|', "\\|"), if c.pass { "PASS" } else { "**FAIL**" }, c.detail.replace('|', "\\|"), c.ms)?;
    }
    writeln!(md, "\n## Mutation check — can the suite fail?\n\nEach mutant breaks one rule in the compiler on purpose. A mutant no case catches is a hole in the suite.\n\n| Mutant | Rule broken | Cases that turned red |\n| :--- | :--- | :--- |")?;
    for (name, what, red) in &caught {
        writeln!(md, "| `{name}` | {what} | {} |", if red.is_empty() { "**none — not caught**".to_string() } else { format!("{} ({})", red.len(), red.join(", ")) })?;
    }
    std::fs::write("results/tqs-coverage.md", md)?;
    for c in &out {
        println!("[{}] {:<4} {:<48} {}", if c.pass { "PASS" } else { "FAIL" }, c.id, c.function, c.detail);
    }
    let uncaught = caught.iter().filter(|c| c.2.is_empty()).count();
    println!("\n{passed}/{} passed; {}/{} mutants caught; written to results/tqs-coverage.md", out.len(), caught.len() - uncaught, caught.len());
    if uncaught > 0 {
        out.push(CaseResult { id: "MUT".into(), area: "mutation", function: format!("{uncaught} mutant(s) not caught"), reference: "—".into(), pass: false, detail: String::new(), ms: 0.0 });
    }
    Ok(out)
}
