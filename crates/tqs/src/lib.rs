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
/// Bound of every time point (epoch ms, 9999-12-31) and duration: sums and differences of a few
/// of them, as the compiler forms (`from - offset - window`), can never overflow an i64.
pub const MAX_MS: i64 = 253_402_300_799_999;

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
    ("outer_scalar_base_first", "formula join outer of scalars keeps only the first operand's steps"),
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
    /// (to − from) and `$__interval` (step), usable as windows. `"$$text"` is the literal
    /// `"$text"` (see [`resolve_var`]).
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

/// Default step: the smallest round step giving at most ~300 points. Round steps line up with
/// each other (15 s, 2 min, 5 min, 1 h are all multiples of 15 s), so a dashboard read at any
/// range can use points materialized at a finer resolution.
pub fn default_step_ms(range_ms: i64) -> i64 {
    const STEPS: [i64; 21] = [
        1_000,
        2_000,
        5_000,
        10_000,
        15_000,
        30_000,
        60_000,
        120_000,
        180_000,
        300_000,
        600_000,
        900_000,
        1_800_000,
        3_600_000,
        7_200_000,
        10_800_000,
        21_600_000,
        43_200_000,
        86_400_000,
        172_800_000,
        604_800_000,
    ];
    let want = range_ms / 300;
    STEPS.into_iter().find(|s| *s >= want).unwrap_or_else(|| (want + 86_399_999) / 86_400_000 * 86_400_000)
}

/// Why a query cannot be materialized, or `None`. Materialized points are computed in slices and
/// stitched together, so every output point must depend only on its own timestamp.
pub fn materialize_blocker(tqs: &Tqs) -> Option<&'static str> {
    if tqs.kind != Kind::Timeseries {
        return Some("only timeseries queries can be materialized");
    }
    if tqs.top.as_ref().is_some_and(|t| t.rank != "per_step" && t.query.is_none()) {
        return Some("top with rank max, avg or last ranks over the whole range");
    }
    if tqs.queries.iter().any(|q| q.fill != Fill::None) {
        return Some("fill depends on which series appear in the range");
    }
    let built_in = |s: &str| s.contains("__range") || s.contains("__interval");
    if tqs
        .queries
        .iter()
        .any(|q| q.func.as_ref().and_then(|f| f.window.as_deref()).is_some_and(built_in) || q.matchers.iter().any(|m| built_in(&m.value)))
    {
        return Some("$__range and $__interval depend on the requested range");
    }
    if tqs.queries.iter().any(|q| q.spans.is_some() || q.logs.is_some()) {
        return Some("spans and logs queries are not materialized yet");
    }
    None
}

static LABEL_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-zA-Z_][a-zA-Z0-9_]*$").expect("static regex"));
static METRIC_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-zA-Z_:][a-zA-Z0-9_:]*$").expect("static regex"));
static QUERY_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z][a-z0-9_]{0,31}$").expect("static regex"));
static ATTR_FIELD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(attr|resource)\.[A-Za-z_][A-Za-z0-9_.\-/:]{0,127}$").expect("static regex"));
static TRACE_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9a-f]{32}$").expect("static regex"));

pub const SPAN_FIELDS: &[&str] = &["service_name", "span_name", "span_kind", "status_code", "duration_ms", "region", "trace_id"];
pub const LOG_FIELDS: &[&str] = &["service_name", "severity_number", "severity_text", "body", "pattern_hash", "trace_id", "region"];
/// Columns a spans / logs query may group by (besides `attr.*` / `resource.*`).
pub const SPAN_GROUP_FIELDS: &[&str] = &["service_name", "span_name", "span_kind", "status_code", "region"];
pub const LOG_GROUP_FIELDS: &[&str] = &["service_name", "severity_number", "severity_text", "pattern_hash", "region"];
pub const TRACE_FIELDS: &[&str] = &["duration_ms", "has_error", "span_count", "root.service_name", "root.span_name", "service"];

/// Parses `30s`, `5m`, `2h`, `7d` (positive whole units only, at most [`MAX_MS`]).
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
    n.checked_mul(mult).filter(|ms| n > 0 && *ms <= MAX_MS)
}

/// `${name}` / `$name` group references are refused in derive replacements (only `$1`..`$9`).
static NAMED_GROUP_REF: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\$(\{|[a-zA-Z_])").expect("static regex"));
static VAR_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z][a-z0-9_]{0,31}$").expect("static regex"));

/// `"$name"` → the variable's value; `"$$text"` → the literal `"$text"`; anything else
/// unchanged. Built-ins resolve to durations. `None`: an undefined variable.
pub fn resolve_var(v: &str, vars: &std::collections::BTreeMap<String, String>, r: &Resolved) -> Option<String> {
    match v.strip_prefix('$') {
        None => Some(v.to_string()),
        Some(lit) if lit.starts_with('$') => Some(lit.to_string()),
        Some("__range") => Some(format!("{}ms", r.to_ms - r.from_ms)),
        Some("__interval") => Some(format!("{}ms", r.step_ms)),
        Some(name) => vars.get(name).cloned(),
    }
}

/// A condition with a string `"$name"` value resolved (see [`resolve_var`]); `None`: an
/// undefined variable. Applies to spans/logs conditions, not to `search.trace`.
pub fn resolve_cond(c: &Cond, vars: &std::collections::BTreeMap<String, String>, r: &Resolved) -> Option<Cond> {
    match c.value.as_str() {
        Some(v) => Some(Cond { value: serde_json::Value::String(resolve_var(v, vars, r)?), ..c.clone() }),
        None => Some(c.clone()),
    }
}

fn resolve_point(p: &TimePoint, now_ms: i64) -> Option<i64> {
    let ms = match p {
        TimePoint::EpochMs(ms) => *ms,
        TimePoint::Text(t) if t == "now" => now_ms,
        TimePoint::Text(t) => now_ms.checked_sub(t.strip_prefix('-').and_then(parse_duration_ms)?)?,
    };
    (0..=MAX_MS).contains(&ms).then_some(ms)
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
                        None => Some(default_step_ms(range)),
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
                _ => push(
                    "time",
                    "time".into(),
                    "from/to must be epoch ms, \"now\" or \"-<duration>\" like \"-2h\", between 1970 and 9999".into(),
                ),
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
                    validate_var_cond(c, fields, &format!("{path}[{i}]"), &self.vars, &resolved, &mut push);
                }
                let orders: &[&str] = if self.kind == Kind::TraceSearch { &["duration_desc", "start_desc"] } else { &["desc", "asc"] };
                if let Some(o) = s.order.as_deref().filter(|o| !orders.contains(o)) {
                    push("schema", "search.order".into(), format!("{o:?}: order is one of {}", orders.join(", ")));
                }
                for (i, c) in s.trace.iter().enumerate() {
                    validate_cond(c, TRACE_FIELDS, &format!("search.trace[{i}]"), &mut push);
                    // Whole-trace fields with a narrower grammar than validate_cond's text/number rules.
                    match (c.field.as_str(), c.op.as_str()) {
                        ("service", "=" | "=~") | ("has_error", "=") | ("root.service_name" | "root.span_name", "=" | "!=" | "=~") => {}
                        ("root.service_name" | "root.span_name", _) => {
                            push("cond", format!("search.trace[{i}]"), "root fields take =, != or =~".into())
                        }
                        ("duration_ms" | "span_count", "=" | "!=" | ">" | ">=" | "<" | "<=") => {}
                        ("duration_ms" | "span_count", _) => {
                            push("cond", format!("search.trace[{i}]"), "duration_ms and span_count take =, !=, >, >=, <, <=".into())
                        }
                        ("service", _) => push("cond", format!("search.trace[{i}]"), "service takes = or =~".into()),
                        ("has_error", _) => push("cond", format!("search.trace[{i}]"), "has_error takes = true|false".into()),
                        _ => {}
                    }
                    if c.field == "has_error" && !c.value.is_boolean() {
                        push("cond", format!("search.trace[{i}].value"), "has_error compares with true or false".into());
                    }
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
                        // A `$var` regex is checked as the value it resolves to.
                        "=~" | "!~" => {
                            validate_regex(&resolve_var(&m.value, &self.vars, r).unwrap_or_default(), &format!("{mp}.value"), push)
                        }
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
                    if f.op == FnOp::HistogramQuantile {
                        push(
                            "then",
                            format!("{path}.then"),
                            "then does not apply after histogram_quantile: group the quantile with agg.by (having works without then)"
                                .into(),
                        );
                    }
                    if let Some(l) = duplicate(t.by.iter().flatten()) {
                        push("label", format!("{path}.then.by"), format!("{l:?} is listed twice"));
                    }
                    let first: Vec<String> = q.agg.as_ref().and_then(|a| a.by.clone()).unwrap_or_default();
                    for l in t.by.iter().flatten() {
                        if q.agg.as_ref().is_some_and(|a| a.by.is_some()) && !first.contains(l) {
                            push("then", format!("{path}.then.by"), format!("{l:?} is not among agg.by, so it no longer exists"));
                        }
                    }
                }
            } else {
                let (conds, fields, key) = match (&q.spans, &q.logs) {
                    (Some(c), _) => (c, SPAN_FIELDS, "spans"),
                    (_, Some(c)) => (c, LOG_FIELDS, "logs"),
                    _ => unreachable!("one source checked above"),
                };
                for (j, c) in conds.iter().enumerate() {
                    validate_var_cond(c, fields, &format!("{path}.{key}[{j}]"), &self.vars, r, push);
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
                if let Some(l) = duplicate(a.by.iter().flatten()).or_else(|| duplicate(a.without.iter().flatten())) {
                    push("label", format!("{path}.agg"), format!("{l:?} is listed twice"));
                }
                for l in a.by.iter().chain(a.without.iter()).flatten() {
                    let ok = if q.metric.is_some() {
                        LABEL_NAME.is_match(l)
                    } else {
                        let fields = if q.spans.is_some() { SPAN_GROUP_FIELDS } else { LOG_GROUP_FIELDS };
                        fields.contains(&l.as_str()) || ATTR_FIELD.is_match(l)
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
                    if ast.refs().iter().all(|r| r == "time") {
                        push("formula", format!("{path}.expr"), "a formula needs at least one query (time alone has no series)".into());
                    }
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

/// The first value that occurs twice.
fn duplicate<'a>(it: impl Iterator<Item = &'a String>) -> Option<&'a String> {
    let mut seen = HashSet::new();
    it.into_iter().find(|l| !seen.insert(*l))
}

fn validate_regex(r: &str, path: &str, push: &mut impl FnMut(&'static str, String, String)) {
    if r.len() > MAX_REGEX_LEN {
        push("regex", path.into(), format!("regex longer than {MAX_REGEX_LEN}"));
    } else if let Err(e) = Regex::new(r) {
        push("regex", path.into(), format!("invalid RE2 regex: {e}"));
    }
}

/// A spans/logs condition, checked as the value its `$var` resolves to (a regex variable is
/// checked as the regex it holds).
fn validate_var_cond(
    c: &Cond,
    fields: &[&str],
    path: &str,
    vars: &std::collections::BTreeMap<String, String>,
    r: &Resolved,
    push: &mut impl FnMut(&'static str, String, String),
) {
    match resolve_cond(c, vars, r) {
        Some(resolved) => validate_cond(&resolved, fields, path, push),
        None => push("var", format!("{path}.value"), format!("{} is not a defined variable", c.value)),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn codes(doc: serde_json::Value) -> Vec<&'static str> {
        match Tqs::parse(&doc.to_string()).map_err(|p| vec![p]).and_then(|t| t.validate(1_000_000_000).map(|_| ())) {
            Ok(()) => vec![],
            Err(ps) => ps.into_iter().map(|p| p.code).collect(),
        }
    }

    fn events(by: &str) -> serde_json::Value {
        serde_json::json!({"tqs": "3", "id": "x", "kind": "timeseries", "time": {"from": "-1h"},
            "queries": [{"name": "a", "spans": [], "measure": {"op": "count"}, "agg": {"by": [by]}}]})
    }

    #[test]
    fn groupable_fields_are_per_signal() {
        assert!(codes(events("service_name")).is_empty());
        assert!(codes(events("resource.k8s.pod")).is_empty(), "resource.* is bound, so it is groupable");
        assert_eq!(codes(events("duration_ms")), vec!["label"]);
        assert_eq!(codes(events("severity_text")), vec!["label"], "a logs field is not a spans column");
    }

    #[test]
    fn formulas_need_a_query() {
        let doc = serde_json::json!({"tqs": "3", "id": "x", "kind": "timeseries", "time": {"from": "-1h"},
            "queries": [{"name": "a", "metric": "m", "fn": {"op": "last"}}], "formulas": [{"name": "t", "expr": "time"}]});
        assert_eq!(codes(doc), vec!["formula"]);
    }

    #[test]
    fn variable_regexes_are_checked_resolved() {
        let doc = serde_json::json!({"tqs": "3", "id": "x", "kind": "timeseries", "time": {"from": "-1h"}, "vars": {"re": "(unclosed"},
            "queries": [{"name": "a", "metric": "m", "fn": {"op": "last"}, "match": [{"label": "l", "op": "=~", "value": "$re"}]}]});
        assert_eq!(codes(doc), vec!["regex"]);
    }

    #[test]
    fn list_kinds_check_order_and_vars() {
        let doc = |extra: serde_json::Value| {
            let mut d = serde_json::json!({"tqs": "3", "id": "x", "kind": "log_search", "time": {"from": "-1h"}, "search": {"logs": [{"field": "service_name", "op": "=", "value": "$svc"}]}});
            for (k, v) in extra.as_object().expect("object") {
                d[k] = v.clone();
            }
            d
        };
        assert_eq!(codes(doc(serde_json::json!({}))), vec!["var"], "an undefined $var is refused");
        assert!(codes(doc(serde_json::json!({"vars": {"svc": "api"}}))).is_empty());
        let mut bad = doc(serde_json::json!({"vars": {"svc": "api"}}));
        bad["search"]["order"] = serde_json::json!("sideways");
        assert_eq!(codes(bad), vec!["schema"]);
    }

    #[test]
    fn trace_conditions_have_their_own_grammar() {
        let doc = |c: serde_json::Value| serde_json::json!({"tqs": "3", "id": "x", "kind": "trace_search", "time": {"from": "-1h"}, "search": {"trace": [c]}});
        assert!(codes(doc(serde_json::json!({"field": "service", "op": "=", "value": "api"}))).is_empty());
        assert!(codes(doc(serde_json::json!({"field": "has_error", "op": "=", "value": true}))).is_empty());
        assert!(!codes(doc(serde_json::json!({"field": "service", "op": "!=", "value": "api"}))).is_empty());
        assert!(!codes(doc(serde_json::json!({"field": "has_error", "op": "=", "value": "true"}))).is_empty());
        assert!(
            !codes(doc(serde_json::json!({"field": "root.span_name", "op": "contains", "value": "x"}))).is_empty(),
            "contains never matched a root field"
        );
    }

    fn metric_doc(q: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"tqs": "3", "id": "x", "kind": "timeseries", "time": {"from": "-1h"}, "queries": [q]})
    }

    #[test]
    fn condition_regex_variables_are_checked_resolved() {
        let doc = |re: &str| {
            serde_json::json!({"tqs": "3", "id": "x", "kind": "timeseries", "time": {"from": "-1h"}, "vars": {"re": re},
                "queries": [{"name": "a", "logs": [{"field": "body", "op": "=~", "value": "$re"}], "measure": {"op": "count"}}]})
        };
        assert_eq!(codes(doc("(unclosed")), vec!["regex"], "a query condition");
        assert!(codes(doc("time.*")).is_empty());
        let search = serde_json::json!({"tqs": "3", "id": "x", "kind": "log_search", "time": {"from": "-1h"}, "vars": {"re": "(unclosed"},
            "search": {"logs": [{"field": "body", "op": "!~", "value": "$re"}]}});
        assert_eq!(codes(search), vec!["regex"], "a search condition");
    }

    #[test]
    fn double_dollar_is_a_literal_dollar() {
        let r = Resolved { from_ms: 0, to_ms: 60_000, step_ms: 1_000 };
        let vars = [("home".to_string(), "/root".to_string())].into();
        assert_eq!(resolve_var("$home", &vars, &r).as_deref(), Some("/root"));
        assert_eq!(resolve_var("$$home", &vars, &r).as_deref(), Some("$home"));
        assert_eq!(resolve_var("$$", &vars, &r).as_deref(), Some("$"));
        assert_eq!(resolve_var("a$b", &vars, &r).as_deref(), Some("a$b"), "only a leading $ is special");
        // Undefined `$HOME` is refused; `$$HOME` searches for the text `$HOME`.
        let search = |v: &str| {
            serde_json::json!({"tqs": "3", "id": "x", "kind": "log_search", "time": {"from": "-1h"},
            "search": {"logs": [{"field": "body", "op": "contains", "value": v}]}})
        };
        assert_eq!(codes(search("$HOME")), vec!["var"]);
        assert!(codes(search("$$HOME")).is_empty());
        let doc = metric_doc(
            serde_json::json!({"name": "a", "metric": "m", "fn": {"op": "last"}, "match": [{"label": "l", "op": "=", "value": "$$x"}]}),
        );
        let t = Tqs::parse(&doc.to_string()).expect("parses");
        let r = t.validate(1_000_000_000).expect("valid");
        let (_, params) = compile::timeseries_sql(&t, &r).expect("compiles");
        assert!(params.contains(&duckdb::types::Value::Text("$x".into())), "bound as the literal $x: {params:?}");
    }

    #[test]
    fn huge_times_and_durations_are_problems_not_panics() {
        let base = |time: serde_json::Value, window: &str| {
            serde_json::json!({"tqs": "3", "id": "x", "kind": "timeseries", "time": time,
                "queries": [{"name": "a", "metric": "m", "fn": {"op": "rate", "window": window}}]})
        };
        let ok = serde_json::json!({"from": "-1h"});
        assert_eq!(codes(base(serde_json::json!({"from": i64::MIN, "to": i64::MAX}), "5m")), vec!["time"]);
        assert_eq!(codes(base(serde_json::json!({"from": "-106751991167301d"}), "5m")), vec!["time"], "n * unit overflows");
        assert_eq!(codes(base(serde_json::json!({"from": "-9223372036854775807ms"}), "5m")), vec!["time"]);
        assert_eq!(
            codes(base(serde_json::json!({"from": "-1h", "step": "18014398509481984w"}), "5m")),
            vec!["step"],
            "n * unit wraps to 0"
        );
        assert_eq!(codes(base(serde_json::json!({"from": "-1h", "step": "9223372036854775807ms"}), "5m")), vec!["step"]);
        assert_eq!(codes(base(ok.clone(), "106751991167301d")), vec!["window"]);
        assert_eq!(parse_duration_ms("18014398509481984w"), None);
        assert_eq!(parse_duration_ms("7d"), Some(604_800_000));
        let mut off = base(ok, "5m");
        off["queries"][0]["offset"] = serde_json::json!("106751991167301d");
        assert_eq!(codes(off), vec!["offset"]);
    }

    #[test]
    fn duplicate_group_labels_are_refused() {
        let q = |agg: serde_json::Value| metric_doc(serde_json::json!({"name": "a", "metric": "m", "fn": {"op": "last"}, "agg": agg}));
        assert_eq!(codes(q(serde_json::json!({"op": "sum", "by": ["job", "job"]}))), vec!["label"]);
        assert_eq!(codes(q(serde_json::json!({"op": "sum", "without": ["job", "job"]}))), vec!["label"]);
        assert!(codes(q(serde_json::json!({"op": "sum", "by": ["job", "instance"]}))).is_empty());
        let mut then = q(serde_json::json!({"op": "sum", "by": ["job", "instance"]}));
        then["queries"][0]["then"] = serde_json::json!({"op": "max", "by": ["job", "job"]});
        assert_eq!(codes(then), vec!["label"]);
        let events = serde_json::json!({"tqs": "3", "id": "x", "kind": "timeseries", "time": {"from": "-1h"},
            "queries": [{"name": "a", "spans": [], "measure": {"op": "count"}, "agg": {"by": ["service_name", "service_name"]}}]});
        assert_eq!(codes(events), vec!["label"]);
    }

    #[test]
    fn then_is_refused_after_histogram_quantile() {
        let hist = |then: Option<serde_json::Value>, having: Option<serde_json::Value>| {
            let mut q = serde_json::json!({"name": "a", "metric": "lat", "fn": {"op": "histogram_quantile", "q": 0.99, "window": "5m"},
                "agg": {"op": "sum", "by": ["service"]}});
            if let Some(t) = then {
                q["then"] = t;
            }
            if let Some(h) = having {
                q["having"] = h;
            }
            metric_doc(q)
        };
        assert_eq!(codes(hist(Some(serde_json::json!({"op": "max"})), None)), vec!["then"]);
        assert_eq!(codes(hist(Some(serde_json::json!({"op": "max"})), Some(serde_json::json!({"op": ">", "value": 1.0})))), vec!["then"]);
        assert!(codes(hist(None, Some(serde_json::json!({"op": ">", "value": 1.0})))).is_empty(), "having alone filters the quantiles");
    }

    #[test]
    fn default_steps_are_round_and_line_up() {
        assert_eq!(default_step_ms(15 * 60_000), 5_000);
        assert_eq!(default_step_ms(3_600_000), 15_000);
        assert_eq!(default_step_ms(6 * 3_600_000), 120_000);
        assert_eq!(default_step_ms(12 * 3_600_000), 180_000);
        assert_eq!(default_step_ms(24 * 3_600_000), 300_000);
        assert_eq!(default_step_ms(7 * 86_400_000), 3_600_000);
        for h in [1, 6, 24, 168] {
            assert_eq!(default_step_ms(h * 3_600_000) % 15_000, 0, "{h}h lines up with a 15 s resolution");
        }
    }

    #[test]
    fn what_can_be_materialized() {
        let doc = |extra: &str| {
            Tqs::parse(&format!(
                r#"{{"tqs":"3","id":"x","kind":"timeseries","time":{{"from":"-1h"}},"queries":[{{"name":"a","metric":"m","fn":{{"op":"rate","window":"5m"}}{extra}}}]}}"#
            ))
            .expect("fixture")
        };
        assert_eq!(materialize_blocker(&doc("")), None);
        assert!(materialize_blocker(&doc(r#","fill":"zero""#)).is_some());
        let range = Tqs::parse(r#"{"tqs":"3","id":"x","kind":"timeseries","time":{"from":"-1h"},"queries":[{"name":"a","metric":"m","fn":{"op":"increase","window":"$__range"}}]}"#).expect("fixture");
        assert!(materialize_blocker(&range).is_some());
        let top = Tqs::parse(r#"{"tqs":"3","id":"x","kind":"timeseries","time":{"from":"-1h"},"queries":[{"name":"a","metric":"m","fn":{"op":"last"}}],"top":{"n":3,"rank":"max"}}"#).expect("fixture");
        assert!(materialize_blocker(&top).is_some());
        let per_step = Tqs::parse(r#"{"tqs":"3","id":"x","kind":"timeseries","time":{"from":"-1h"},"queries":[{"name":"a","metric":"m","fn":{"op":"last"}}],"top":{"n":3}}"#).expect("fixture");
        assert_eq!(materialize_blocker(&per_step), None);
    }
}
