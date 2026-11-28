//! Data generators and dashboard panels shared by the benchmark binaries.

#![allow(dead_code)]

use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue, any_value};
use opentelemetry_proto::tonic::resource::v1::Resource;
use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span, Status};
use rand::{RngExt, rngs::StdRng};
use server::receivers::{Label, Sample, TimeSeries, WriteRequest, encode_remote_write};
use std::sync::Arc;
use std::time::Instant;

pub const SCRAPE: i64 = 15_000;

pub fn pct(xs: &mut [f64], p: f64) -> f64 {
    xs.sort_by(|a, b| a.total_cmp(b));
    xs[((xs.len() as f64 - 1.0) * p).round() as usize]
}

pub fn label(k: &str, v: &str) -> Label {
    Label { name: k.into(), value: v.into() }
}

/// The label sets of the benchmark's series: (name, labels) — counters, gauges, histogram buckets.
pub fn series_set(n: usize) -> Vec<Vec<Label>> {
    let mut out = vec![];
    for i in 0..n {
        out.push(vec![
            label("__name__", "http_requests_total"),
            label("job", &format!("api-{}", i % 10)),
            label("instance", &format!("10.0.{}.{}:9100", i / 250, i % 250)),
            label("route", &format!("/r/{}", i % 20)),
            label("code", if i % 7 == 0 { "500" } else { "200" }),
        ]);
    }
    for i in 0..n / 4 {
        out.push(vec![
            label("__name__", "process_resident_memory_bytes"),
            label("job", &format!("api-{}", i % 10)),
            label("instance", &format!("10.0.{}.{}:9100", i / 250, i % 250)),
        ]);
    }
    for j in 0..10 {
        for le in ["0.005", "0.01", "0.025", "0.05", "0.1", "0.25", "0.5", "1", "2.5", "5", "10", "+Inf"] {
            out.push(vec![label("__name__", "http_request_duration_seconds_bucket"), label("job", &format!("api-{j}")), label("le", le)]);
        }
    }
    out
}

pub fn value(labels: &[Label], idx: usize, k: i64) -> f64 {
    let name = &labels[0].value;
    if name == "process_resident_memory_bytes" {
        return 1e8 + ((idx as i64 * 7919 + k * 13) % 1000) as f64 * 1e5;
    }
    if name.ends_with("_bucket") {
        let le = &labels[2].value;
        let frac = match le.as_str() {
            "+Inf" => 1.0,
            l => (l.parse::<f64>().unwrap().ln() + 6.0).clamp(0.0, 8.5) / 8.5,
        };
        return (k as f64 * 20.0 * frac).floor();
    }
    (k as f64 * (1.0 + (idx % 13) as f64)).floor()
}

/// Remote-write `from..to` (scrape steps) for every series, `clients` requests in flight.
pub fn backfill(
    base: &str,
    sets: &[Vec<Label>],
    t0: i64,
    steps: std::ops::Range<i64>,
    per_request_steps: i64,
    clients: usize,
) -> (usize, f64, Vec<f64>) {
    let slices: Vec<i64> = steps.clone().step_by(per_request_steps as usize).collect();
    let next = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let started = Instant::now();
    let mut handles = vec![];
    // Requests carry ≤ ~2,000 series × steps, like a Prometheus remote-write shard.
    let shard = 2000;
    for _ in 0..clients {
        let (base, sets, slices, next) = (base.to_string(), sets.to_vec(), slices.clone(), next.clone());
        let end = steps.end;
        handles.push(std::thread::spawn(move || {
            let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
            let mut lat = vec![];
            let mut samples = 0;
            loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                // Work items are (time slice, shard), in time order.
                let shards = sets.len().div_ceil(shard);
                if i >= slices.len() * shards {
                    break;
                }
                let (slice, sh) = (slices[i / shards], i % shards);
                let ks: Vec<i64> = (slice..(slice + per_request_steps).min(end)).collect();
                let req = WriteRequest {
                    timeseries: sets
                        .iter()
                        .enumerate()
                        .skip(sh * shard)
                        .take(shard)
                        .map(|(idx, l)| TimeSeries {
                            labels: l.clone(),
                            samples: ks.iter().map(|k| Sample { value: value(l, idx, *k), timestamp: t0 + k * SCRAPE }).collect(),
                        })
                        .collect(),
                };
                samples += req.timeseries.iter().map(|t| t.samples.len()).sum::<usize>();
                let body = encode_remote_write(&req);
                let t = Instant::now();
                loop {
                    let r = agent.post(&format!("{base}/api/v1/write")).header("Content-Encoding", "snappy").send(&body[..]).unwrap();
                    match r.status().as_u16() {
                        204 => break,
                        429 => std::thread::sleep(std::time::Duration::from_millis(50)),
                        s => panic!("remote-write HTTP {s}"),
                    }
                }
                lat.push(t.elapsed().as_secs_f64() * 1000.0);
            }
            (samples, lat)
        }));
    }
    let mut total = 0;
    let mut lat = vec![];
    for h in handles {
        let (s, l) = h.join().unwrap();
        total += s;
        lat.extend(l);
    }
    (total, started.elapsed().as_secs_f64(), lat)
}

pub fn query(base: &str, doc: &serde_json::Value) -> (f64, usize) {
    let t = Instant::now();
    let mut r = ureq::post(&format!("{base}/api/v1/query")).header("Content-Type", "application/json").send(doc.to_string()).unwrap();
    let body = r.body_mut().with_config().limit(1 << 30).read_to_string().unwrap();
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let points = v["series"].as_array().map(|s| s.iter().map(|x| x["points"].as_array().map_or(0, |p| p.len())).sum()).unwrap_or(0)
        + v["rows"].as_array().map_or(0, |r| r.len());
    (ms, points)
}

pub fn panels(from: i64, to: i64, step: &str) -> Vec<(&'static str, serde_json::Value)> {
    let t = serde_json::json!({"from": from, "to": to, "step": step});
    let doc = |id: &str, kind: &str, queries: serde_json::Value, extra: serde_json::Value| {
        let mut d = serde_json::json!({"tqs": "3", "id": id, "kind": kind, "time": t, "queries": queries});
        if let serde_json::Value::Object(m) = extra {
            for (k, v) in m {
                d[k] = v;
            }
        }
        d
    };
    vec![
        (
            "sum by (job) (rate(http_requests_total[5m]))",
            doc(
                "q1",
                "timeseries",
                serde_json::json!([{"name": "r", "metric": "http_requests_total", "fn": {"op": "rate", "window": "5m"}, "agg": {"op": "sum", "by": ["job"]}}]),
                serde_json::json!({}),
            ),
        ),
        (
            "topk(10, sum by (route) (rate(…[5m])))",
            doc(
                "q2",
                "timeseries",
                serde_json::json!([{"name": "r", "metric": "http_requests_total", "fn": {"op": "rate", "window": "5m"}, "agg": {"op": "sum", "by": ["route"]}}]),
                serde_json::json!({"top": {"n": 10}}),
            ),
        ),
        (
            "error ratio formula (2 queries)",
            doc(
                "q3",
                "timeseries",
                serde_json::json!([
            {"name": "e", "metric": "http_requests_total", "match": [{"label": "code", "op": "=", "value": "500"}], "fn": {"op": "rate", "window": "5m"}, "agg": {"op": "sum"}},
            {"name": "a", "metric": "http_requests_total", "fn": {"op": "rate", "window": "5m"}, "agg": {"op": "sum"}}]),
                serde_json::json!({"formulas": [{"name": "pct", "expr": "100 * e / a"}]}),
            ),
        ),
        (
            "histogram_quantile(0.99, … by (job, le))",
            doc(
                "q4",
                "timeseries",
                serde_json::json!([{"name": "p", "metric": "http_request_duration_seconds", "fn": {"op": "histogram_quantile", "q": 0.99, "window": "5m"}, "agg": {"op": "sum", "by": ["job"]}}]),
                serde_json::json!({}),
            ),
        ),
        (
            "max by (job) (gauge)",
            doc(
                "q5",
                "timeseries",
                serde_json::json!([{"name": "g", "metric": "process_resident_memory_bytes", "fn": {"op": "last"}, "agg": {"op": "max", "by": ["job"]}}]),
                serde_json::json!({}),
            ),
        ),
        (
            "one series: rate(…{instance=\"10.0.0.1:9100\"})",
            doc(
                "q6",
                "timeseries",
                serde_json::json!([{"name": "r", "metric": "http_requests_total", "match": [{"label": "instance", "op": "=", "value": "10.0.0.1:9100"}], "fn": {"op": "rate", "window": "5m"}}]),
                serde_json::json!({}),
            ),
        ),
        ("table: sum by (code) (increase(…[$__range]))", {
            let mut d = doc(
                "q7",
                "table",
                serde_json::json!([{"name": "r", "metric": "http_requests_total", "fn": {"op": "increase", "window": "$__range"}, "agg": {"op": "sum", "by": ["code"]}}]),
                serde_json::json!({}),
            );
            d["time"] = serde_json::json!({"from": from, "to": to});
            d
        }),
    ]
}

pub fn kv(k: &str, v: &str) -> KeyValue {
    KeyValue { key: k.into(), value: Some(AnyValue { value: Some(any_value::Value::StringValue(v.into())) }), ..Default::default() }
}

/// `n` traces of 6 spans across 3 services, starts spread over `[from, to)`.
pub fn traces_request(rng: &mut StdRng, n: usize, from_ns: i64, to_ns: i64) -> (ExportTraceServiceRequest, Vec<[u8; 16]>) {
    let services = ["gateway", "checkout", "payment"];
    let mut by_svc: Vec<Vec<Span>> = vec![vec![]; 3];
    let mut ids = vec![];
    for _ in 0..n {
        let tid: [u8; 16] = rng.random();
        ids.push(tid);
        let start = rng.random_range(from_ns..to_ns) as u64;
        let mut parent: Vec<u8> = vec![];
        for j in 0..6u64 {
            let sid: [u8; 8] = rng.random();
            let svc = (j as usize) / 2;
            let st = start + j * 2_000_000;
            by_svc[svc].push(Span {
                trace_id: tid.to_vec(),
                span_id: sid.to_vec(),
                parent_span_id: parent.clone(),
                name: format!("{} op{}", services[svc], j % 2),
                kind: if j == 0 { 2 } else { 1 },
                start_time_unix_nano: st,
                end_time_unix_nano: st + rng.random_range(1_000_000..50_000_000),
                attributes: vec![kv("http.route", &format!("/r/{}", rng.random_range(0..20)))],
                status: Some(Status { code: if rng.random_bool(0.02) { 2 } else { 0 }, message: String::new() }),
                ..Default::default()
            });
            parent = sid.to_vec();
        }
    }
    let req = ExportTraceServiceRequest {
        resource_spans: by_svc
            .into_iter()
            .enumerate()
            .map(|(i, spans)| ResourceSpans {
                resource: Some(Resource { attributes: vec![kv("service.name", services[i])], ..Default::default() }),
                scope_spans: vec![ScopeSpans { spans, ..Default::default() }],
                ..Default::default()
            })
            .collect(),
    };
    (req, ids)
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Where and when this report was produced, as a block at the top of the report: the proof of
/// one run. GitHub Actions runs link to the run; a local run says so and names the machine.
pub fn run_block() -> String {
    let sh = |cmd: &str, args: &[&str]| -> String {
        std::process::Command::new(cmd)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };
    let env = |k: &str| std::env::var(k).unwrap_or_default();
    let commit = sh("git", &["rev-parse", "--short=10", "HEAD"]);
    let dirty = if sh("git", &["status", "--porcelain", "--untracked-files=no"]).is_empty() { "" } else { " (uncommitted changes)" };
    let (cpu, mem_bytes) = if cfg!(target_os = "macos") {
        (sh("sysctl", &["-n", "machdep.cpu.brand_string"]), sh("sysctl", &["-n", "hw.memsize"]).parse::<u64>().unwrap_or(0))
    } else {
        let info = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
        let cpu = info.lines().find_map(|l| l.strip_prefix("model name").map(|v| v.trim_start_matches([' ', '\t', ':']).to_string()));
        let mem = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
        let kb =
            mem.lines().find_map(|l| l.strip_prefix("MemTotal:")).and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok());
        (cpu.unwrap_or_default(), kb.unwrap_or(0) * 1024)
    };
    let where_ = if env("GITHUB_ACTIONS") == "true" {
        format!(
            "GitHub Actions, `{}` runner ([run {}]({}/{}/actions/runs/{}))",
            env("RUNNER_OS").to_lowercase(),
            env("GITHUB_RUN_ID"),
            env("GITHUB_SERVER_URL"),
            env("GITHUB_REPOSITORY"),
            env("GITHUB_RUN_ID")
        )
    } else {
        "local machine".to_string()
    };
    format!(
        "> **Run:** {} UTC · commit `{commit}`{dirty} · {where_} · {} logical CPUs ({}), {:.0} GB RAM, {}\n\n",
        sh("date", &["-u", "+%Y-%m-%d %H:%M"]),
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
        if cpu.is_empty() { "unknown CPU" } else { &cpu },
        mem_bytes as f64 / (1u64 << 30) as f64,
        std::env::consts::OS,
    )
}
