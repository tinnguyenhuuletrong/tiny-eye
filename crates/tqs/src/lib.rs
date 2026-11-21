//! TQS 3 — the dashboard / query schema (SPEC §8). Seeded from the research code.
//! A query is JSON that is validated, then
//! compiled to DuckDB SQL with every user value bound as a parameter.
//!
//! The model is deliberately closed (`deny_unknown_fields`): an agent that invents a
//! field gets a validation error naming it, not a silently ignored option.

pub mod compile;
pub mod formula;

use regex::Regex;
use serde::Deserialize;
use std::collections::HashSet;
use std::sync::LazyLock;

pub const VERSION: &str = "3";
pub const MAX_QUERIES: usize = 10;
pub const MAX_POINTS: i64 = 11_000;
pub const MAX_REGEX_LEN: usize = 256;
pub const LOOKBACK_MS: i64 = 300_000;

/// Mutation switch for the conformance suite only (`--features mutants`): each named mutant
/// breaks one semantic rule on purpose, to prove the suite turns red when that rule is wrong.
/// Without the feature `mutant()` is a constant `false` and the branches compile away.
#[cfg(feature = "mutants")]
pub static MUTANT: std::sync::RwLock<String> = std::sync::RwLock::new(String::new());

#[cfg(feature = "mutants")]
pub fn mutant(name: &str) -> bool {
    MUTANT.read().map(|m| *m == name).unwrap_or(false)
}

#[cfg(not(feature = "mutants"))]
#[inline(always)]
pub fn mutant(_name: &str) -> bool {
    false
}

pub const MUTANTS: &[(&str, &str)] = &[
    ("no_reset_correction", "counter resets are not added back in rate/increase"),
    ("closed_window", "range windows include their left edge [t-w, t]"),
    ("no_extrapolation", "rate/increase/delta use the raw first-to-last difference"),
    ("lookback_4m", "latest-value lookback is 4 m instead of 5 m"),
    ("hist_upper_bound", "histogram_quantile returns the bucket upper bound, no interpolation"),
    ("without_ignored", "agg.without keeps every label"),
    ("fill_ignored", "fill: zero has no effect"),
    ("division_ieee", "x/0 yields +Inf/NaN instead of no value"),
    ("bucket_closed_left", "spans/logs buckets include their left edge"),
    ("derive_ignored", "derive (label_replace) rules are not applied"),
    ("then_ignored", "the second aggregation (then) is skipped"),
    ("outer_as_inner", "formula join outer behaves like inner"),
    ("top_query_ignored", "top.query does not restrict the output"),
    ("absent_unlabelled", "absent() returns no labels"),
    ("time_in_ms", "formula `time` is in milliseconds, not seconds"),
];

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Tqs {
    pub tqs: String,
    pub id: String,
    /// Display only; the engine never reads it.
    #[serde(default)]
    #[allow(dead_code)]
    pub title: Option<String>,
    pub kind: Kind,
    #[serde(default)]
    pub time: Option<TimeSpec>,
    #[serde(default)]
    pub queries: Vec<NamedQuery>,
    #[serde(default)]
    pub formulas: Vec<Formula>,
    #[serde(default)]
    pub top: Option<Top>,
    #[serde(default)]
    pub search: Option<Search>,
    #[serde(default)]
    pub trace_id: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
    /// Dashboard variables. A string value `"$name"` anywhere a value is accepted is
    /// replaced by `vars.name` and bound as a parameter. Built-ins: `$__range`
    /// (to − from) and `$__interval` (step), usable as windows.
    #[serde(default)]
    pub vars: std::collections::BTreeMap<String, String>,
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// One value per series per step.
    Timeseries,
    /// One value per series at `time.to` (an instant query).
    Table,
    TraceSearch,
    Trace,
    LogSearch,
    LogPatterns,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct TimeSpec {
    pub from: TimePoint,
    #[serde(default = "TimePoint::now")]
    pub to: TimePoint,
    #[serde(default)]
    pub step: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(untagged)]
pub enum TimePoint {
    EpochMs(i64),
    Text(String),
}

impl TimePoint {
    fn now() -> Self {
        TimePoint::Text("now".into())
    }
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct NamedQuery {
    pub name: String,
    #[serde(default)]
    pub metric: Option<String>,
    #[serde(default)]
    pub spans: Option<Vec<Cond>>,
    #[serde(default)]
    pub logs: Option<Vec<Cond>>,
    /// Label matchers for `metric` (PromQL `=`, `!=`, `=~`, `!~`).
    #[serde(default, rename = "match")]
    pub matchers: Vec<Matcher>,
    #[serde(default, rename = "fn")]
    pub func: Option<MetricFn>,
    #[serde(default)]
    pub measure: Option<Measure>,
    #[serde(default)]
    pub agg: Option<Agg>,
    #[serde(default)]
    pub offset: Option<String>,
    #[serde(default)]
    pub having: Option<Having>,
    #[serde(default)]
    pub fill: Fill,
    /// Label rewrite before aggregation (PromQL `label_replace`), applied in order.
    #[serde(default)]
    pub derive: Vec<Derive>,
    /// Second aggregation over the first one's groups (PromQL `sum by (a) (max by (a, b) (...))`).
    #[serde(default)]
    pub then: Option<Agg>,
    /// Computed but not returned (e.g. only used by `top.query`).
    #[serde(default)]
    pub hidden: bool,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Derive {
    pub label: String,
    pub from: String,
    pub regex: String,
    pub replacement: String,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Matcher {
    pub label: String,
    pub op: String,
    pub value: String,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Cond {
    pub field: String,
    pub op: String,
    pub value: serde_json::Value,
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FnOp {
    Rate,
    Increase,
    Irate,
    Delta,
    Resets,
    Changes,
    Last,
    AvgOverTime,
    MinOverTime,
    MaxOverTime,
    SumOverTime,
    CountOverTime,
    LastOverTime,
    HistogramQuantile,
}

impl FnOp {
    pub fn needs_window(self) -> bool {
        !matches!(self, FnOp::Last)
    }
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct MetricFn {
    pub op: FnOp,
    #[serde(default)]
    pub window: Option<String>,
    #[serde(default)]
    pub q: Option<f64>,
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AggOp {
    Sum,
    Avg,
    Min,
    Max,
    Count,
    /// 1 at every step where the selection has no series (PromQL `absent`).
    Absent,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Agg {
    #[serde(default)]
    pub op: Option<AggOp>,
    #[serde(default)]
    pub by: Option<Vec<String>>,
    #[serde(default)]
    pub without: Option<Vec<String>>,
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MeasureOp {
    Count,
    Rate,
    Quantile,
    Avg,
    Min,
    Max,
    Sum,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Measure {
    pub op: MeasureOp,
    #[serde(default)]
    pub field: Option<String>,
    #[serde(default)]
    pub q: Option<f64>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Having {
    pub op: String,
    pub value: f64,
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Fill {
    #[default]
    None,
    Zero,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Formula {
    pub name: String,
    pub expr: String,
    /// `inner` (default): a point exists where every vector operand has one.
    /// `outer`: the union of the operands' points; a missing operand is NULL, so the
    /// expression decides (`coalesce(a, b * 0)` is PromQL `a or b * 0`).
    #[serde(default)]
    pub join: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Top {
    pub n: u32,
    /// Rank by another (usually hidden) query's value at each step and keep only those
    /// keys in every output (PromQL `x and on(k) topk(n, y)`).
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default = "Top::default_rank")]
    pub rank: String,
    #[serde(default = "Top::default_order")]
    pub order: String,
}

impl Top {
    fn default_rank() -> String {
        "per_step".into()
    }
    fn default_order() -> String {
        "desc".into()
    }
}

#[derive(Deserialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct Search {
    /// trace_search: at least one span of the trace matches all of these.
    #[serde(default)]
    pub spans: Vec<Cond>,
    /// trace_search: conditions on the trace as a whole (`duration_ms`, `has_error`, `root.*`).
    #[serde(default)]
    pub trace: Vec<Cond>,
    /// log_search / log_patterns.
    #[serde(default)]
    pub logs: Vec<Cond>,
    #[serde(default)]
    pub order: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub code: &'static str,
    pub path: String,
    pub message: String,
}

/// Times resolved against a caller-supplied `now` so tests are deterministic.
#[derive(Debug, Clone, Copy)]
pub struct Resolved {
    pub from_ms: i64,
    pub to_ms: i64,
    pub step_ms: i64,
}

static LABEL_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-zA-Z_][a-zA-Z0-9_]*$").expect("static regex"));
static METRIC_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-zA-Z_:][a-zA-Z0-9_:]*$").expect("static regex"));
static QUERY_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z][a-z0-9_]{0,31}$").expect("static regex"));
static ATTR_FIELD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(attr|resource)\.[A-Za-z_][A-Za-z0-9_.\-/:]{0,127}$").expect("static regex"));
static TRACE_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9a-f]{32}$").expect("static regex"));

pub const SPAN_FIELDS: &[&str] = &["service_name", "span_name", "span_kind", "status_code", "duration_ms", "region", "trace_id"];
pub const LOG_FIELDS: &[&str] = &["service_name", "severity_number", "severity_text", "body", "pattern_hash", "trace_id", "region"];
pub const TRACE_FIELDS: &[&str] = &["duration_ms", "has_error", "span_count", "root.service_name", "root.span_name", "service"];

/// Parses `30s`, `5m`, `2h`, `7d` (positive whole units only).
pub fn parse_duration_ms(s: &str) -> Option<i64> {
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit())?);
    let n: i64 = num.parse().ok()?;
    let mult = match unit {
        "ms" => 1,
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        "w" => 604_800_000,
        _ => return None,
    };
    (n > 0).then_some(n * mult)
}

/// `${name}` / `$name` group references are refused in derive replacements (only `$1`..`$9`).
static NAMED_GROUP_REF: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\$(\{|[a-zA-Z_])").expect("static regex"));
static VAR_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z][a-z0-9_]{0,31}$").expect("static regex"));

/// `"$name"` → the variable's value; anything else unchanged. Built-ins resolve to durations.
pub fn resolve_var(v: &str, vars: &std::collections::BTreeMap<String, String>, r: &Resolved) -> Option<String> {
    match v.strip_prefix('$') {
        None => Some(v.to_string()),
        Some("__range") => Some(format!("{}ms", r.to_ms - r.from_ms)),
        Some("__interval") => Some(format!("{}ms", r.step_ms)),
        Some(name) => vars.get(name).cloned(),
    }
}

fn resolve_point(p: &TimePoint, now_ms: i64) -> Option<i64> {
    match p {
        TimePoint::EpochMs(ms) => Some(*ms),
        TimePoint::Text(t) if t == "now" => Some(now_ms),
        TimePoint::Text(t) => t.strip_prefix('-').and_then(parse_duration_ms).map(|d| now_ms - d),
    }
}

impl Tqs {
    pub fn parse(json: &str) -> Result<Tqs, Problem> {
        serde_json::from_str(json).map_err(|e| Problem { code: "schema", path: "$".into(), message: e.to_string() })
    }

    pub fn query(&self, name: &str) -> Option<&NamedQuery> {
        self.queries.iter().find(|q| q.name == name)
    }

    /// Every problem, not only the first: an agent fixes all of them in one round trip.
    pub fn validate(&self, now_ms: i64) -> Result<Resolved, Vec<Problem>> {
        let mut p = vec![];
        let mut push = |code: &'static str, path: String, message: String| p.push(Problem { code, path, message });
        if self.tqs != VERSION {
            push("version", "tqs".into(), format!("unsupported version {:?}; this engine speaks \"{VERSION}\"", self.tqs));
        }
        if !QUERY_NAME.is_match(&self.id.replace('-', "_")) {
            push("id", "id".into(), "id must match ^[a-z][a-z0-9_-]{0,31}$".into());
        }
        let mut resolved = Resolved { from_ms: 0, to_ms: 0, step_ms: 0 };
        let needs_time = self.kind != Kind::Trace;
        match (&self.time, needs_time) {
            (None, true) => push("time", "time".into(), "time is required for this kind".into()),
            (Some(t), _) => match (resolve_point(&t.from, now_ms), resolve_point(&t.to, now_ms)) {
                (Some(f), Some(to)) if f < to || (self.kind == Kind::Table && f <= to) => {
                    let range = to - f;
                    let step = match &t.step {
                        Some(s) => parse_duration_ms(s),
                        // Default: ~300 points, at least 1 s.
                        None => Some(((range / 300).max(1_000) + 999) / 1_000 * 1_000),
                    };
                    match step {
                        None => push("step", "time.step".into(), format!("bad duration {:?}", t.step)),
                        Some(s) if self.kind == Kind::Timeseries && range / s + 1 > MAX_POINTS => push(
                            "too_many_points",
                            "time.step".into(),
                            format!("{} points per series; the limit is {MAX_POINTS}: raise step", range / s + 1),
                        ),
                        Some(s) => resolved = Resolved { from_ms: f, to_ms: to, step_ms: s },
                    }
                }
                (Some(_), Some(_)) => push("time_order", "time".into(), "from must be before to".into()),
                _ => push("time", "time".into(), "from/to must be epoch ms, \"now\" or \"-<duration>\" like \"-2h\"".into()),
            },
            _ => {}
        }
        for (k, v) in &self.vars {
            if !VAR_NAME.is_match(k) || k.starts_with("__") || v.len() > 256 {
                push("var", format!("vars.{k}"), "variable names match ^[a-z][a-z0-9_]{0,31}$; values are at most 256 characters".into());
            }
        }
        match self.kind {
            Kind::Timeseries | Kind::Table => self.validate_queries(&mut push, &resolved),
            Kind::Trace => match &self.trace_id {
                Some(id) if TRACE_ID.is_match(id) => {}
                _ => push("trace_id", "trace_id".into(), "trace_id must be 32 lowercase hex characters".into()),
            },
            Kind::TraceSearch | Kind::LogSearch | Kind::LogPatterns => {
                let s = self.search.clone().unwrap_or_default();
                let (conds, fields, path): (Vec<&Cond>, &[&str], &str) = match self.kind {
                    Kind::TraceSearch => (s.spans.iter().collect(), SPAN_FIELDS, "search.spans"),
                    _ => (s.logs.iter().collect(), LOG_FIELDS, "search.logs"),
                };
                for (i, c) in conds.iter().enumerate() {
                    validate_cond(c, fields, &format!("{path}[{i}]"), &mut push);
                }
                for (i, c) in s.trace.iter().enumerate() {
                    validate_cond(c, TRACE_FIELDS, &format!("search.trace[{i}]"), &mut push);
                }
                if let Some(l) = self.limit
                    && (l == 0 || l > 5_000)
                {
                    push("limit", "limit".into(), "limit must be 1..=5000".into());
                }
            }
        }
        if p.is_empty() { Ok(resolved) } else { Err(p) }
    }

    fn validate_queries(&self, push: &mut impl FnMut(&'static str, String, String), r: &Resolved) {
        let var_ok = |v: &str| resolve_var(v, &self.vars, r).is_some();
        if self.queries.is_empty() || self.queries.len() > MAX_QUERIES {
            push("queries", "queries".into(), format!("1..={MAX_QUERIES} queries required"));
        }
        let mut names = HashSet::new();
        for (i, q) in self.queries.iter().enumerate() {
            let path = format!("queries[{i}]");
            if !QUERY_NAME.is_match(&q.name) || q.name == "time" || !names.insert(q.name.clone()) {
                push("name", format!("{path}.name"), format!("{:?} must be unique and match ^[a-z][a-z0-9_]{{0,31}}$", q.name));
            }
            let sources = [q.metric.is_some(), q.spans.is_some(), q.logs.is_some()].iter().filter(|b| **b).count();
            if sources != 1 {
                push("source", path.clone(), "exactly one of metric, spans, logs".into());
                continue;
            }
            if let Some(m) = &q.metric {
                if !METRIC_NAME.is_match(m) {
                    push("metric", format!("{path}.metric"), format!("{m:?} is not a metric name"));
                }
                let Some(f) = &q.func else {
                    push("fn", format!("{path}.fn"), "metrics queries need fn (use {\"op\":\"last\"} for the latest value)".into());
                    continue;
                };
                match (&f.window, f.op.needs_window()) {
                    (None, true) => push("window", format!("{path}.fn.window"), format!("{:?} needs a window", f.op)),
                    (Some(w), _) if resolve_var(w, &self.vars, r).as_deref().and_then(parse_duration_ms).is_none() => {
                        push("window", format!("{path}.fn.window"), format!("bad duration {w:?} (or an unknown variable)"))
                    }
                    (Some(_), false) => push(
                        "window",
                        format!("{path}.fn.window"),
                        "last uses the fixed 5m lookback; remove window or use last_over_time".into(),
                    ),
                    _ => {}
                }
                match (f.op, f.q) {
                    (FnOp::HistogramQuantile, Some(q)) if (0.0..=1.0).contains(&q) => {}
                    (FnOp::HistogramQuantile, _) => push("q", format!("{path}.fn.q"), "histogram_quantile needs q in [0, 1]".into()),
                    (_, Some(_)) => push("q", format!("{path}.fn.q"), "q only applies to histogram_quantile".into()),
                    _ => {}
                }
                for (j, m) in q.matchers.iter().enumerate() {
                    let mp = format!("{path}.match[{j}]");
                    if !LABEL_NAME.is_match(&m.label) {
                        push("label", format!("{mp}.label"), format!("{:?} is not a label name", m.label));
                    }
                    if !var_ok(&m.value) {
                        push("var", format!("{mp}.value"), format!("{:?} is not a defined variable", m.value));
                    }
                    match m.op.as_str() {
                        "=" | "!=" => {}
                        "=~" | "!~" => validate_regex(&m.value, &format!("{mp}.value"), push),
                        o => push("op", format!("{mp}.op"), format!("{o:?}: matchers take =, !=, =~, !~")),
                    }
                }
                if q.measure.is_some() {
                    push("measure", format!("{path}.measure"), "measure is for spans/logs; metrics use fn".into());
                }
                for (j, d) in q.derive.iter().enumerate() {
                    let dp = format!("{path}.derive[{j}]");
                    if !LABEL_NAME.is_match(&d.label) || !LABEL_NAME.is_match(&d.from) {
                        push("label", dp.clone(), "derive.label and derive.from must be label names".into());
                    }
                    validate_regex(&d.regex, &format!("{dp}.regex"), push);
                    if d.replacement.contains('\\') || d.replacement.len() > 256 || NAMED_GROUP_REF.is_match(&d.replacement) {
                        push("derive", format!("{dp}.replacement"), "replacement is literal text with $1..$9 group references".into());
                    }
                }
                let absent = q.agg.as_ref().is_some_and(|a| a.op == Some(AggOp::Absent));
                if absent && (q.agg.as_ref().is_some_and(|a| a.by.is_some() || a.without.is_some()) || q.then.is_some()) {
                    push("agg", format!("{path}.agg"), "absent takes no by/without/then".into());
                }
                if let Some(t) = &q.then {
                    if q.agg.is_none() || t.op.is_none() || t.without.is_some() || t.op == Some(AggOp::Absent) {
                        push("then", format!("{path}.then"), "then needs agg first, an op (not absent) and optionally by".into());
                    }
                    let first: Vec<String> = q.agg.as_ref().and_then(|a| a.by.clone()).unwrap_or_default();
                    for l in t.by.iter().flatten() {
                        if q.agg.as_ref().is_some_and(|a| a.by.is_some()) && !first.contains(l) {
                            push("then", format!("{path}.then.by"), format!("{l:?} is not among agg.by, so it no longer exists"));
                        }
                    }
                }
            } else {
                let (conds, fields) = match (&q.spans, &q.logs) {
                    (Some(c), _) => (c, SPAN_FIELDS),
                    (_, Some(c)) => (c, LOG_FIELDS),
                    _ => unreachable!("one source checked above"),
                };
                for (j, c) in conds.iter().enumerate() {
                    validate_cond(c, fields, &format!("{path}.where[{j}]"), push);
                    if c.value.as_str().is_some_and(|v| !var_ok(v)) {
                        push("var", format!("{path}.where[{j}].value"), format!("{} is not a defined variable", c.value));
                    }
                }
                if q.then.is_some() || !q.derive.is_empty() {
                    push("then", path.clone(), "then and derive are for metrics".into());
                }
                match &q.measure {
                    None => push("measure", format!("{path}.measure"), "spans/logs queries need measure".into()),
                    Some(m) => {
                        let field_ok = m.field.as_deref().is_none_or(|f| f == "duration_ms") && (q.spans.is_some() || m.field.is_none());
                        let needs_field =
                            matches!(m.op, MeasureOp::Quantile | MeasureOp::Avg | MeasureOp::Min | MeasureOp::Max | MeasureOp::Sum);
                        if needs_field && (m.field.is_none() || q.logs.is_some()) || !field_ok {
                            push(
                                "measure",
                                format!("{path}.measure"),
                                "quantile/avg/min/max/sum need field \"duration_ms\" on spans".into(),
                            );
                        }
                        match (m.op, m.q) {
                            (MeasureOp::Quantile, Some(q)) if (0.0..=1.0).contains(&q) => {}
                            (MeasureOp::Quantile, _) => push("q", format!("{path}.measure.q"), "quantile needs q in [0, 1]".into()),
                            (_, Some(_)) => push("q", format!("{path}.measure.q"), "q only applies to quantile".into()),
                            _ => {}
                        }
                    }
                }
                if q.func.is_some() || !q.matchers.is_empty() {
                    push("fn", path.clone(), "fn and match are for metrics; spans/logs use where + measure".into());
                }
                if let Some(a) = &q.agg
                    && (a.op.is_some() || a.without.is_some())
                {
                    push("agg", format!("{path}.agg"), "spans/logs aggregate with measure; agg takes only by".into());
                }
            }
            if let Some(a) = &q.agg {
                if a.by.is_some() && a.without.is_some() {
                    push("agg", format!("{path}.agg"), "by and without are exclusive".into());
                }
                if q.metric.is_some() && a.op.is_none() {
                    push("agg", format!("{path}.agg.op"), "agg.op is required for metrics".into());
                }
                for l in a.by.iter().chain(a.without.iter()).flatten() {
                    let ok = if q.metric.is_some() {
                        LABEL_NAME.is_match(l)
                    } else {
                        SPAN_FIELDS.contains(&l.as_str()) || LOG_FIELDS.contains(&l.as_str()) || ATTR_FIELD.is_match(l)
                    };
                    if !ok {
                        push("label", format!("{path}.agg"), format!("{l:?} cannot be grouped on"));
                    }
                }
            }
            if let Some(o) = &q.offset
                && parse_duration_ms(o).is_none()
            {
                push("offset", format!("{path}.offset"), format!("bad duration {o:?} (positive only)"));
            }
            if let Some(h) = &q.having
                && (!["=", "!=", ">", ">=", "<", "<="].contains(&h.op.as_str()) || !h.value.is_finite())
            {
                push("having", format!("{path}.having"), "having takes =, !=, >, >=, <, <= and a finite number".into());
            }
        }
        let mut known: Vec<String> = self.queries.iter().map(|q| q.name.clone()).collect();
        known.push("time".into()); // evaluation time in seconds (PromQL time())
        for (i, f) in self.formulas.iter().enumerate() {
            let path = format!("formulas[{i}]");
            if f.join.as_deref().is_some_and(|j| j != "inner" && j != "outer") {
                push("formula", format!("{path}.join"), "join is inner or outer".into());
            }
            if !QUERY_NAME.is_match(&f.name) || known.contains(&f.name) {
                push("name", format!("{path}.name"), format!("{:?} must be unique and match ^[a-z][a-z0-9_]{{0,31}}$", f.name));
            }
            match formula::parse(&f.expr) {
                Err(e) => push("formula", format!("{path}.expr"), e),
                Ok(ast) => {
                    for r in ast.refs() {
                        if !known.contains(&r) {
                            push("formula_ref", format!("{path}.expr"), format!("{r:?} is not a query or an earlier formula"));
                        }
                    }
                    if let Err(e) = compile::formula_shape(self, &ast) {
                        push("formula_join", format!("{path}.expr"), e);
                    }
                }
            }
            known.push(f.name.clone());
        }
        if let Some(t) = &self.top {
            if t.query.as_deref().is_some_and(|q| self.query(q).is_none()) {
                push("top", "top.query".into(), "top.query must name a query".into());
            }
            if t.n == 0
                || t.n > 1_000
                || !["per_step", "max", "avg", "last"].contains(&t.rank.as_str())
                || !["asc", "desc"].contains(&t.order.as_str())
            {
                push("top", "top".into(), "top.n 1..=1000, rank per_step|max|avg|last, order asc|desc".into());
            }
        }
    }
}

fn validate_regex(r: &str, path: &str, push: &mut impl FnMut(&'static str, String, String)) {
    if r.len() > MAX_REGEX_LEN {
        push("regex", path.into(), format!("regex longer than {MAX_REGEX_LEN}"));
    } else if let Err(e) = Regex::new(r) {
        push("regex", path.into(), format!("invalid RE2 regex: {e}"));
    }
}

fn validate_cond(c: &Cond, fields: &[&str], path: &str, push: &mut impl FnMut(&'static str, String, String)) {
    // attr.* / resource.* exist on spans and logs, not on whole-trace conditions.
    let attr = ATTR_FIELD.is_match(&c.field) && fields != TRACE_FIELDS;
    if !(fields.contains(&c.field.as_str()) || attr) {
        push("field", format!("{path}.field"), format!("{:?} is not a field of this signal", c.field));
        return;
    }
    let numeric = ["span_kind", "status_code", "duration_ms", "severity_number", "pattern_hash", "span_count"].contains(&c.field.as_str());
    match (c.op.as_str(), &c.value) {
        ("=" | "!=", v) if numeric && v.is_number() || !numeric && (v.is_string() || v.is_boolean()) => {}
        (">" | ">=" | "<" | "<=", v) if numeric && v.is_number() => {}
        ("=~" | "!~", serde_json::Value::String(r)) if !numeric => validate_regex(r, &format!("{path}.value"), push),
        ("contains", serde_json::Value::String(_)) if !numeric => {}
        ("in", serde_json::Value::Array(a))
            if !a.is_empty() && a.len() <= 100 && a.iter().all(|v| if numeric { v.is_number() } else { v.is_string() }) => {}
        ("exists", serde_json::Value::Bool(_)) if attr => {}
        (op, v) => push(
            "cond",
            path.into(),
            format!("{op:?} with {v} is not valid for {} field {:?}", if numeric { "numeric" } else { "text" }, c.field),
        ),
    }
}
