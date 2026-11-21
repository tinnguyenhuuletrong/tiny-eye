//! Deterministic fixtures for the TQS suite, shaped like Margince's own `/metrics`
//! (AI-call counters by provider/task/outcome, a latency histogram with Margince's
//! bounds, queue-depth gauges), plus spans and logs with known edge cases.
//!
//! Every edge case is planted on purpose and named here, so a test failure points at it:
//! counter reset, a series that starts late, one that stops, missing scrapes, jittered
//! timestamps, a label value with a quote, a provider with no error series, a histogram
//! whose p99 falls in the +Inf bucket, a histogram reset, a trace with a lost parent span.

use crate::synth::{self, Span};
use crate::write;
use anyhow::Result;
use arrow::array::{
    Array, ArrayRef, FixedSizeBinaryBuilder, Float64Array, Int64Array, MapBuilder, RecordBatch, StringArray, StringBuilder,
    TimestampNanosecondArray, UInt8Array, UInt64Array,
};
use arrow::datatypes::{DataType, Field, Schema, TimeUnit};
use duckdb::Connection;
use rand::{Rng, SeedableRng, rngs::StdRng};
use std::collections::BTreeMap;
use std::sync::Arc;

pub const T0: i64 = 1_762_740_000_000; // 2025-11-10T02:00:00Z
pub const SCRAPE_MS: i64 = 15_000;
pub const SPAN_MS: i64 = 2 * 3_600_000;
pub const LATENCY_BOUNDS: [f64; 11] = [0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0];

#[derive(Clone, Debug)]
pub struct Series {
    pub metric: String,
    pub labels: BTreeMap<String, String>,
    pub points: Vec<(i64, f64)>,
}

impl Series {
    pub fn id(&self) -> u64 {
        let canon: String = self.labels.iter().map(|(k, v)| format!("{k}={v}\u{1}")).collect();
        write::series_id("fixture", &self.metric, &canon)
    }
}

fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

/// Scrape timestamps with whole-second jitter (exact in both engines) and ~3 % gaps.
fn scrape_times(rng: &mut StdRng, from: i64, to: i64) -> Vec<i64> {
    let mut out = vec![];
    let mut k = 0;
    loop {
        let t = T0 + k * SCRAPE_MS + rng.random_range(0..3) * 1_000;
        k += 1;
        if t > to {
            break;
        }
        if t >= from && !rng.random_bool(0.03) {
            out.push(t);
        }
    }
    out
}

pub fn metrics() -> Vec<Series> {
    let mut rng = StdRng::seed_from_u64(42);
    let mut out = vec![];
    let end = T0 + SPAN_MS;
    // Counters.
    for provider in ["anthropic", "openai"] {
        let model = if provider == "anthropic" { "sonnet" } else { "gpt" };
        for task in ["summarize", "classify", "draft", "quote\"d"] {
            for outcome in ["ok", "error"] {
                // Planted: openai has no classify error series at all (fill-zero case).
                if provider == "openai" && task == "classify" && outcome == "error" {
                    continue;
                }
                let mut from = T0;
                let mut to = end;
                if (provider, task, outcome) == ("openai", "draft", "error") {
                    from = T0 + 65 * 60_000; // starts late
                }
                if (provider, task, outcome) == ("openai", "classify", "ok") {
                    to = T0 + 90 * 60_000; // stops
                }
                let per_s = match (outcome, task) {
                    ("error", "classify") if provider == "anthropic" => 0.0, // flat zero counter
                    ("error", _) => rng.random_range(0.02..0.2),
                    _ => rng.random_range(0.5..3.0),
                };
                let mut v: f64 = rng.random_range(0.0..5_000.0f64).floor();
                let mut pts = vec![];
                for t in scrape_times(&mut rng, from, to) {
                    if (provider, task, outcome) == ("anthropic", "summarize", "ok")
                        && t > T0 + 50 * 60_000
                        && pts.last().is_some_and(|(pt, _)| *pt <= T0 + 50 * 60_000)
                    {
                        v = rng.random_range(0.0..20.0f64).floor(); // process restart
                    } else if !pts.is_empty() {
                        v += (per_s * 15.0 * rng.random_range(0.5..1.5f64)).floor();
                    }
                    pts.push((t, v));
                }
                out.push(Series {
                    metric: "margince_ai_calls_total".into(),
                    labels: labels(&[("provider", provider), ("model", model), ("task", task), ("tier", "standard"), ("outcome", outcome)]),
                    points: pts,
                });
            }
        }
    }
    // Histogram with Margince's latency bounds.
    for provider in ["anthropic", "openai"] {
        for task in ["summarize", "classify", "draft"] {
            let slow = (provider, task) == ("openai", "draft"); // half the calls exceed 10 s
            let median = if provider == "anthropic" { 0.8 } else { 0.5 };
            let mut buckets = [0u64; 12];
            let (mut count, mut sum) = (0u64, 0f64);
            let times = scrape_times(&mut rng, T0, end);
            let mut rows: Vec<(i64, [u64; 12], u64, f64)> = vec![];
            for (i, t) in times.iter().enumerate() {
                if (provider, task) == ("anthropic", "classify") && *t > T0 + 50 * 60_000 && times[i - 1] <= T0 + 50 * 60_000 {
                    buckets = [0; 12];
                    count = 0;
                    sum = 0.0;
                }
                if i > 0 {
                    for _ in 0..rng.random_range(5..40) {
                        let d = if slow && rng.random_bool(0.5) {
                            12.0 + rng.random_range(0.0..3.0)
                        } else {
                            let z: f64 = (-2.0 * rng.random::<f64>().max(1e-12).ln()).sqrt()
                                * (2.0 * std::f64::consts::PI * rng.random::<f64>()).cos();
                            median * (0.9 * z).exp()
                        };
                        for (b, ub) in LATENCY_BOUNDS.iter().enumerate() {
                            if d <= *ub {
                                buckets[b] += 1;
                            }
                        }
                        buckets[11] += 1;
                        count += 1;
                        sum += d;
                    }
                }
                rows.push((*t, buckets, count, (sum * 1e6).round() / 1e6));
            }
            for b in 0..12 {
                let le = if b == 11 { "+Inf".to_string() } else { format!("{}", LATENCY_BOUNDS[b]) };
                out.push(Series {
                    metric: "margince_ai_call_duration_seconds_bucket".into(),
                    labels: labels(&[("provider", provider), ("task", task), ("le", &le)]),
                    points: rows.iter().map(|r| (r.0, r.1[b] as f64)).collect(),
                });
            }
            out.push(Series {
                metric: "margince_ai_call_duration_seconds_count".into(),
                labels: labels(&[("provider", provider), ("task", task)]),
                points: rows.iter().map(|r| (r.0, r.2 as f64)).collect(),
            });
            out.push(Series {
                metric: "margince_ai_call_duration_seconds_sum".into(),
                labels: labels(&[("provider", provider), ("task", task)]),
                points: rows.iter().map(|r| (r.0, r.3)).collect(),
            });
        }
    }
    // Gauges.
    for queue in ["default", "ai", "mail", "sweep"] {
        let to = if queue == "sweep" { T0 + 70 * 60_000 } else { end };
        let mut v: f64 = rng.random_range(0..50) as f64;
        let pts = scrape_times(&mut rng, T0, to)
            .into_iter()
            .map(|t| {
                v = (v + rng.random_range(-6..7) as f64).max(0.0);
                (t, v)
            })
            .collect();
        out.push(Series { metric: "margince_job_queue_depth".into(), labels: labels(&[("queue", queue)]), points: pts });
    }
    for state in ["idle", "in_use"] {
        let pts = scrape_times(&mut rng, T0, end).into_iter().map(|t| (t, (rng.random_range(0.0..20.0f64) * 4.0).round() / 4.0)).collect();
        out.push(Series { metric: "margince_pgxpool_conns".into(), labels: labels(&[("state", state)]), points: pts });
    }
    out
}

fn om_escape(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

/// OpenMetrics text for `promtool tsdb create-blocks-from openmetrics`. Families are
/// contiguous and each series' samples ascend, which the importer requires.
pub fn openmetrics(series: &[Series]) -> String {
    let mut by_family: BTreeMap<String, Vec<&Series>> = BTreeMap::new();
    for s in series {
        let fam =
            ["_bucket", "_count", "_sum"].iter().fold(s.metric.clone(), |m, suf| m.strip_suffix(suf).map(str::to_string).unwrap_or(m));
        by_family.entry(fam).or_default().push(s);
    }
    let mut out = String::new();
    for (_, members) in by_family {
        for s in members {
            let l: Vec<String> = s.labels.iter().map(|(k, v)| format!("{k}=\"{}\"", om_escape(v))).collect();
            for (t, v) in &s.points {
                out.push_str(&format!("{}{{{}}} {} {}\n", s.metric, l.join(","), v, t / 1000));
            }
        }
    }
    out.push_str("# EOF\n");
    out
}

fn map_column(entries: &[BTreeMap<String, String>]) -> Result<arrow::array::MapArray> {
    let mut b = MapBuilder::new(None, StringBuilder::new(), StringBuilder::new());
    for e in entries {
        for (k, v) in e {
            b.keys().append_value(k);
            b.values().append_value(v);
        }
        b.append(true)?;
    }
    Ok(b.finish())
}

fn load(conn: &Connection, table: &str, batch: &RecordBatch, dir: &std::path::Path, extra_cols: &str) -> Result<()> {
    let path = dir.join(format!("{table}.parquet"));
    std::fs::write(&path, write::parquet_bytes(batch, None, 65_536)?)?;
    conn.execute_batch(&format!("CREATE OR REPLACE TABLE {table} AS SELECT *{extra_cols} FROM read_parquet('{}')", path.display()))?;
    Ok(())
}

pub fn load_metrics(conn: &Connection, series: &[Series], dir: &std::path::Path) -> Result<()> {
    let label_maps: Vec<BTreeMap<String, String>> = series.iter().map(|s| s.labels.clone()).collect();
    let map = map_column(&label_maps)?;
    let ser = RecordBatch::try_new(
        Arc::new(Schema::new(vec![
            Field::new("series_id", DataType::UInt64, false),
            Field::new("metric_name", DataType::Utf8, false),
            Field::new("labels", map.data_type().clone(), false),
        ])),
        vec![
            Arc::new(UInt64Array::from_iter_values(series.iter().map(Series::id))),
            Arc::new(StringArray::from_iter_values(series.iter().map(|s| s.metric.as_str()))),
            Arc::new(map) as ArrayRef,
        ],
    )?;
    load(conn, "metrics_series", &ser, dir, "")?;
    let mut ids = vec![];
    let mut ts = vec![];
    let mut vs = vec![];
    for s in series {
        for (t, v) in &s.points {
            ids.push(s.id());
            ts.push(*t);
            vs.push(*v);
        }
    }
    let pts = RecordBatch::try_new(
        Arc::new(Schema::new(vec![
            Field::new("series_id", DataType::UInt64, false),
            Field::new("timestamp_ms", DataType::Int64, false),
            Field::new("value", DataType::Float64, false),
        ])),
        vec![Arc::new(UInt64Array::from(ids)), Arc::new(Int64Array::from(ts)), Arc::new(Float64Array::from(vs))],
    )?;
    load(conn, "metrics_points", &pts, dir, "")
}

pub fn span_kind(s: &Span) -> u8 {
    match (s.parent_span_id, s.name) {
        (None, _) => 2,                         // server
        (_, n) if n.starts_with("SELECT") => 3, // client
        _ => 1,                                 // internal
    }
}

/// Spans: synth traces in [T0, T0 + 3 min) shifted to the 2 h fixture range, with one
/// planted orphan (a trace whose checkout span was lost).
pub fn spans() -> (Vec<Span>, [u8; 16]) {
    let mut all = synth::traces(3_000, 5);
    let mut rng = StdRng::seed_from_u64(9);
    for s in all.iter_mut() {
        // synth::traces spreads roots over 3 min; move each whole trace (same shift for
        // all its spans, keyed by trace id) somewhere in the 2 h fixture range.
        let offset = (s.trace_id[0] as i64 * 256 + s.trace_id[1] as i64) * (SPAN_MS - 600_000) / 65_536;
        s.start_ns += (T0 + offset - synth::T0_MS) * 1_000_000;
        if rng.random_bool(0.02) {
            s.http_route = "/api/refund";
        }
    }
    let orphan_trace = all[100].trace_id;
    all.retain(|s| !(s.trace_id == orphan_trace && s.name == "checkout.place"));
    (all, orphan_trace)
}

pub fn load_spans(conn: &Connection, spans: &mut [Span], dir: &std::path::Path) -> Result<()> {
    let batch = write::span_batch(spans, write::SpanLayout::TraceSortedBloom)?;
    load(conn, "spans_raw", &batch, dir, "")?;
    conn.execute_batch(
        "CREATE OR REPLACE TABLE spans AS SELECT *,
            (CASE WHEN parent_span_id IS NULL THEN 2 WHEN span_name LIKE 'SELECT%' THEN 3 ELSE 1 END)::UTINYINT AS span_kind,
            MAP {'deployment.environment': 'prod'} AS resource_attrs
         FROM spans_raw; DROP TABLE spans_raw;",
    )?;
    Ok(())
}

#[derive(Clone, Debug)]
pub struct Log {
    pub ts_ns: i64,
    pub service: &'static str,
    pub severity: u8,
    pub body: String,
    pub trace_id: Option<[u8; 16]>,
}

pub fn severity_text(n: u8) -> &'static str {
    match n {
        17.. => "ERROR",
        13.. => "WARN",
        _ => "INFO",
    }
}

/// Deterministic masking (versioned): the same template always hashes the same, on any
/// node, in any region, after any restart — unlike learned (Drain-style) clustering.
pub fn mask_body(body: &str) -> String {
    static RULES: std::sync::LazyLock<Vec<regex::Regex>> = std::sync::LazyLock::new(|| {
        [
            r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}",
            r"\b\d{1,3}(?:\.\d{1,3}){3}(?::\d+)?\b",
            r"\b[0-9a-fA-F]{8,}\b",
            r#""[^"]*""#,
            r"\d+(?:\.\d+)?",
        ]
        .iter()
        .map(|r| regex::Regex::new(r).expect("static regex"))
        .collect()
    });
    RULES.iter().fold(body.to_string(), |b, r| r.replace_all(&b, "<*>").into_owned())
}

pub fn pattern_hash(body: &str) -> u64 {
    xxhash_rust::xxh64::xxh64(mask_body(body).as_bytes(), 1)
}

pub fn logs(spans: &[Span]) -> Vec<Log> {
    let mut rng = StdRng::seed_from_u64(77);
    let mut out = vec![];
    for s in spans {
        let tid = Some(s.trace_id);
        let ts = s.start_ns + 1_000_000;
        match s.service {
            "checkout" if s.name == "checkout.place" => out.push(Log {
                ts_ns: ts,
                service: "checkout",
                severity: 9,
                body: format!(
                    "user {} placed order {} total {}.{:02}",
                    rng.random_range(1000..9999),
                    rng.random_range(10000..99999),
                    rng.random_range(5..500),
                    rng.random_range(0..100)
                ),
                trace_id: tid,
            }),
            "checkout" if rng.random_bool(0.05) => out.push(Log {
                ts_ns: ts,
                service: "checkout",
                severity: 17,
                body: format!(
                    "connection to 10.0.{}.{}:5432 timed out after {}ms",
                    rng.random_range(0..255),
                    rng.random_range(0..255),
                    rng.random_range(100..5000)
                ),
                trace_id: tid,
            }),
            "payment" if s.status == 2 => out.push(Log {
                ts_ns: ts,
                service: "payment",
                severity: 13,
                body: format!(
                    "payment declined for card ****{} code \"{}\"",
                    rng.random_range(1000..9999),
                    ["insufficient_funds", "expired"][rng.random_range(0..2)]
                ),
                trace_id: tid,
            }),
            "ledger" if rng.random_bool(0.3) => out.push(Log {
                ts_ns: ts,
                service: "ledger",
                severity: 9,
                body: format!(
                    "ledger posted entry {:08x}-{:04x}-4{:03x}-a{:03x}-{:012x}",
                    rng.random::<u32>(),
                    rng.random::<u16>(),
                    rng.random_range(0..4096),
                    rng.random_range(0..4096),
                    rng.random::<u64>() & 0xffff_ffff_ffff
                ),
                trace_id: tid,
            }),
            _ => {}
        }
    }
    // Logs without a trace, including non-ASCII text.
    for i in 0..200 {
        out.push(Log {
            ts_ns: (T0 + i * 36_000) * 1_000_000,
            service: "gateway",
            severity: 9,
            body: format!("health check ok in {}ms — région eu", rng.random_range(1..9)),
            trace_id: None,
        });
    }
    out
}

pub fn load_logs(conn: &Connection, logs: &[Log], dir: &std::path::Path) -> Result<()> {
    let mut tids = FixedSizeBinaryBuilder::with_capacity(logs.len(), 16);
    for l in logs {
        match l.trace_id {
            Some(t) => tids.append_value(t)?,
            None => tids.append_null(),
        }
    }
    let attrs =
        map_column(&logs.iter().map(|l| labels(&[("logger", if l.severity >= 13 { "alerts" } else { "app" })])).collect::<Vec<_>>())?;
    let batch = RecordBatch::try_new(
        Arc::new(Schema::new(vec![
            Field::new("timestamp_ns", DataType::Timestamp(TimeUnit::Nanosecond, None), false),
            Field::new("region", DataType::Utf8, false),
            Field::new("service_name", DataType::Utf8, false),
            Field::new("severity_number", DataType::UInt8, false),
            Field::new("severity_text", DataType::Utf8, false),
            Field::new("pattern_hash", DataType::UInt64, false),
            Field::new("body", DataType::Utf8, false),
            Field::new("trace_id", DataType::FixedSizeBinary(16), true),
            Field::new("log_attrs", attrs.data_type().clone(), false),
        ])),
        vec![
            Arc::new(TimestampNanosecondArray::from_iter_values(logs.iter().map(|l| l.ts_ns))),
            Arc::new(StringArray::from_iter_values(
                logs.iter().map(|l| if l.service == "payment" || l.service == "ledger" { "eu-west" } else { "us-east" }),
            )),
            Arc::new(StringArray::from_iter_values(logs.iter().map(|l| l.service))),
            Arc::new(UInt8Array::from_iter_values(logs.iter().map(|l| l.severity))),
            Arc::new(StringArray::from_iter_values(logs.iter().map(|l| severity_text(l.severity)))),
            Arc::new(UInt64Array::from_iter_values(logs.iter().map(|l| pattern_hash(&l.body)))),
            Arc::new(StringArray::from_iter_values(logs.iter().map(|l| l.body.as_str()))),
            Arc::new(tids.finish()),
            Arc::new(attrs),
        ],
    )?;
    load(conn, "logs", &batch, dir, ", MAP {'deployment.environment': 'prod'} AS resource_attrs")
}

/// Metrics behind the Margince Grafana board's panels, with the board's own labels
/// (`kubernetes_namespace`, per-workspace job gauges, `declared_info` baselines, routes,
/// cAdvisor containers). `ns-b` lacks error series; `ns-empty` exists only in queries.
pub fn dashboard_metrics() -> Vec<Series> {
    let mut rng = StdRng::seed_from_u64(4242);
    let end = T0 + SPAN_MS;
    let mut out = vec![];
    let counter = |rng: &mut StdRng, metric: &str, l: &[(&str, &str)], per_s: f64, from: i64, to: i64| -> Series {
        let mut v: f64 = rng.random_range(0.0..1_000.0f64).floor();
        let mut pts = vec![];
        for t in scrape_times(rng, from, to) {
            if !pts.is_empty() {
                v += (per_s * 15.0 * rng.random_range(0.5..1.5f64)).floor();
            }
            pts.push((t, v));
        }
        Series { metric: metric.into(), labels: labels(l), points: pts }
    };
    for (provider, model) in [("anthropic", "sonnet"), ("anthropic", "haiku"), ("openai", "gpt")] {
        for task in ["summarize", "draft"] {
            for tier in ["standard", "premium"] {
                let per_s = if provider == "openai" { 0.0 } else { rng.random_range(0.2..2.0) }; // openai/gpt: idle model
                out.push(counter(
                    &mut rng,
                    "margince_ai_calls_total",
                    &[("kubernetes_namespace", "ns-a"), ("provider", provider), ("model", model), ("task", task), ("tier", tier)],
                    per_s,
                    T0,
                    end,
                ));
            }
        }
    }
    out.push(counter(
        &mut rng,
        "margince_ai_calls_total",
        &[("kubernetes_namespace", "ns-b"), ("provider", "anthropic"), ("model", "sonnet"), ("task", "summarize"), ("tier", "standard")],
        0.7,
        T0,
        end,
    ));
    for provider in ["anthropic", "openai"] {
        out.push(counter(
            &mut rng,
            "margince_ai_call_attempts_total",
            &[("kubernetes_namespace", "ns-a"), ("provider", provider)],
            2.0,
            T0,
            end,
        ));
        out.push(counter(
            &mut rng,
            "margince_ai_call_errors_total",
            &[("kubernetes_namespace", "ns-a"), ("provider", provider)],
            0.1,
            T0,
            end,
        ));
    }
    out.push(counter(
        &mut rng,
        "margince_ai_call_attempts_total",
        &[("kubernetes_namespace", "ns-b"), ("provider", "anthropic")],
        1.0,
        T0,
        end,
    ));
    // Job gauges per workspace, reported by two pods; (ai, k2) stops at 60 min.
    for (queue, kind) in [("default", "k1"), ("default", "k2"), ("ai", "k1"), ("ai", "k2")] {
        for ws in ["w1", "w2", "w3"] {
            for pod in ["worker-0", "worker-1"] {
                let to = if (queue, kind) == ("ai", "k2") { T0 + 60 * 60_000 } else { end };
                let mut v: f64 = rng.random_range(0..30) as f64;
                let pts = scrape_times(&mut rng, T0, to)
                    .into_iter()
                    .map(|t| {
                        v = (v + rng.random_range(-3..4) as f64).max(0.0);
                        (t, v)
                    })
                    .collect();
                out.push(Series {
                    metric: "margince_job_queue_depth".into(),
                    labels: labels(&[
                        ("kubernetes_namespace", "ns-a"),
                        ("queue", queue),
                        ("kind", kind),
                        ("workspace_id", ws),
                        ("pod", pod),
                    ]),
                    points: pts,
                });
            }
        }
    }
    for (queue, kind) in [("default", "k1"), ("default", "k2"), ("ai", "k1"), ("ai", "k3"), ("", "k9")] {
        let pts = scrape_times(&mut rng, T0, end).into_iter().map(|t| (t, 1.0)).collect();
        out.push(Series {
            metric: "margince_job_declared_info".into(),
            labels: labels(&[("kubernetes_namespace", "ns-a"), ("queue", queue), ("kind", kind)]),
            points: pts,
        });
    }
    // Process start times; api-1 restarts at 40 min.
    for (app, pod) in [("api", "api-0"), ("api", "api-1"), ("worker", "worker-0")] {
        let pts = scrape_times(&mut rng, T0, end)
            .into_iter()
            .map(|t| {
                (
                    t,
                    if pod == "api-1" && t > T0 + 40 * 60_000 {
                        ((T0 + 40 * 60_000) / 1000) as f64
                    } else {
                        ((T0 - 86_400_000) / 1000) as f64
                    },
                )
            })
            .collect();
        out.push(Series {
            metric: "process_start_time_seconds".into(),
            labels: labels(&[("kubernetes_namespace", "ns-a"), ("app", app), ("pod", pod)]),
            points: pts,
        });
    }
    // HTTP latency histogram over 14 routes (the panel keeps the top 10 by request rate).
    for r in 0..14 {
        let route = format!("/v1/r{r:02}");
        let rate = 0.2 + r as f64 * 0.3;
        let median = 0.02 + (r % 5) as f64 * 0.04;
        let mut buckets = [0u64; 12];
        let mut rows = vec![];
        for (i, t) in scrape_times(&mut rng, T0, end).into_iter().enumerate() {
            if i > 0 {
                for _ in 0..((rate * 15.0 * rng.random_range(0.5..1.5f64)) as usize) {
                    let z: f64 =
                        (-2.0 * rng.random::<f64>().max(1e-12).ln()).sqrt() * (2.0 * std::f64::consts::PI * rng.random::<f64>()).cos();
                    let d = median * (0.8 * z).exp();
                    for (b, ub) in LATENCY_BOUNDS.iter().enumerate() {
                        if d <= *ub {
                            buckets[b] += 1;
                        }
                    }
                    buckets[11] += 1;
                }
            }
            rows.push((t, buckets));
        }
        for b in 0..12 {
            let le = if b == 11 { "+Inf".to_string() } else { format!("{}", LATENCY_BOUNDS[b]) };
            out.push(Series {
                metric: "margince_http_request_duration_seconds_bucket".into(),
                labels: labels(&[("kubernetes_namespace", "ns-a"), ("route", &route), ("le", &le)]),
                points: rows.iter().map(|(t, bs)| (*t, bs[b] as f64)).collect(),
            });
        }
        out.push(Series {
            metric: "margince_http_request_duration_seconds_count".into(),
            labels: labels(&[("kubernetes_namespace", "ns-a"), ("route", &route)]),
            points: rows.iter().map(|(t, bs)| (*t, bs[11] as f64)).collect(),
        });
    }
    // cAdvisor CPU: real containers plus the "" (cgroup total) and "POD" rows panels exclude.
    for pod in ["api-application-7f9c-abcde", "worker-application-55d-xyz12", "postgres-master-0", "redis-0"] {
        for container in ["app", "", "POD"] {
            let per_s = match container {
                "app" => rng.random_range(0.05..0.8),
                "" => 1.0,
                _ => 0.001,
            };
            let mut v = 0.0;
            let pts = scrape_times(&mut rng, T0, end)
                .into_iter()
                .map(|t| {
                    v += per_s * 15.0 * rng.random_range(0.8..1.2f64);
                    (t, (v * 1000.0f64).round() / 1000.0)
                })
                .collect();
            out.push(Series {
                metric: "container_cpu_usage_seconds_total".into(),
                labels: labels(&[("namespace", "ns-a"), ("pod", pod), ("container", container)]),
                points: pts,
            });
        }
    }
    out
}
