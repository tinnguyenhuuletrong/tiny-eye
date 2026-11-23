//! `tiny-eye-bench`: ingest, storage, query and trace-lookup numbers for one machine.
//!
//!   tiny-eye-bench [--series 2000] [--hours 6] [--traces 20000] [--s3 http://127.0.0.1:19000] [--out report.md]
//!
//! Runs a real node in process behind its HTTP API (same code path as the binary), on local
//! disk or on S3/MinIO, and writes a Markdown report. Phases:
//!   1. remote-write backfill of `series` counters + gauges + a histogram, 4 concurrent clients
//!   2. flush + checkpoint, bytes per sample (L0)
//!   3. dashboard panels on L0 files, then compaction, panels and bytes per sample on L1
//!   4. live data in the hot tail: the same panels over the last hour
//!   5. OTLP traces in many flushes, then trace lookup by id before and after compaction
//!      (with S3: GET requests counted from DuckDB's HTTP log)

#![allow(clippy::unwrap_used)]

use anyhow::Result;
use engine::node::now_ms;
use engine::{Config, Node};
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue, any_value};
use opentelemetry_proto::tonic::resource::v1::Resource;
use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span, Status};
use prost::Message;
use rand::{RngExt, SeedableRng, rngs::StdRng};
use server::receivers::{Label, Sample, TimeSeries, WriteRequest, encode_remote_write};
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Instant;

const SCRAPE: i64 = 15_000;

const CAVEATS: &str = "\n**Caveats.** Synthetic counters rise linearly, so bytes per sample (0.4) is far below real data (the research measured ~6 B/sample on random walks; measure on real exporter data before sizing). L1 is slightly larger than L0 because compaction adds `positive_delta`. Panel timings are with warm DuckDB caches (`enable_external_file_cache`); the GET column is measured with caches off.\n";

struct Args {
    series: usize,
    hours: i64,
    traces: usize,
    s3: Option<String>,
    out: Option<String>,
}

fn args() -> Args {
    let a: Vec<String> = std::env::args().collect();
    let get = |n: &str| a.iter().position(|x| x == n).and_then(|i| a.get(i + 1)).cloned();
    Args {
        series: get("--series").and_then(|v| v.parse().ok()).unwrap_or(2000),
        hours: get("--hours").and_then(|v| v.parse().ok()).unwrap_or(6),
        traces: get("--traces").and_then(|v| v.parse().ok()).unwrap_or(20_000),
        s3: get("--s3"),
        out: get("--out"),
    }
}

fn pct(xs: &mut [f64], p: f64) -> f64 {
    xs.sort_by(|a, b| a.total_cmp(b));
    xs[((xs.len() as f64 - 1.0) * p).round() as usize]
}

fn label(k: &str, v: &str) -> Label {
    Label { name: k.into(), value: v.into() }
}

/// The label sets of the benchmark's series: (name, labels) — counters, gauges, histogram buckets.
fn series_set(n: usize) -> Vec<Vec<Label>> {
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

fn value(labels: &[Label], idx: usize, k: i64) -> f64 {
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
fn backfill(
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

fn query(base: &str, doc: &serde_json::Value) -> (f64, usize) {
    let t = Instant::now();
    let mut r = ureq::post(&format!("{base}/api/v1/query")).header("Content-Type", "application/json").send(doc.to_string()).unwrap();
    let body = r.body_mut().with_config().limit(1 << 30).read_to_string().unwrap();
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let points = v["series"].as_array().map(|s| s.iter().map(|x| x["points"].as_array().map_or(0, |p| p.len())).sum()).unwrap_or(0)
        + v["rows"].as_array().map_or(0, |r| r.len());
    (ms, points)
}

fn panels(from: i64, to: i64, step: &str) -> Vec<(&'static str, serde_json::Value)> {
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

fn run_panels(base: &str, md: &mut String, title: &str, from: i64, to: i64, step: &str, runs: usize) {
    writeln!(md, "\n### {title}\n\n| Panel | Points | p50 ms | p95 ms | max ms |\n| :--- | ---: | ---: | ---: | ---: |").unwrap();
    for (name, doc) in panels(from, to, step) {
        let _ = query(base, &doc); // warm the caches once
        let mut ms = vec![];
        let mut points = 0;
        for _ in 0..runs {
            let (m, p) = query(base, &doc);
            ms.push(m);
            points = p;
        }
        let (p50, p95, mx) = (pct(&mut ms, 0.5), pct(&mut ms, 0.95), pct(&mut ms, 1.0));
        println!("  {name:<50} {points:>7} pts  p50 {p50:>7.1} ms  p95 {p95:>7.1} ms");
        writeln!(md, "| `{name}` | {points} | {p50:.1} | {p95:.1} | {mx:.1} |").unwrap();
    }
}

fn kv(k: &str, v: &str) -> KeyValue {
    KeyValue { key: k.into(), value: Some(AnyValue { value: Some(any_value::Value::StringValue(v.into())) }), ..Default::default() }
}

/// `n` traces of 6 spans across 3 services, starts spread over `[from, to)`.
fn traces_request(rng: &mut StdRng, n: usize, from_ns: i64, to_ns: i64) -> (ExportTraceServiceRequest, Vec<[u8; 16]>) {
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

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

async fn trace_lookups(node: &Arc<Node>, base: &str, ids: &[[u8; 16]], md: &mut String, title: &str, s3: bool) {
    let files: i64 = node
        .with_conn(|c| Ok(c.query_row("SELECT count(DISTINCT data_file) FROM ducklake_list_files('lake', 'spans')", [], |r| r.get(0))?))
        .await
        .unwrap();
    let mut rows = vec![];
    for (label, id) in [("present", hex(&ids[ids.len() / 2])), ("absent", "0123456789abcdef0123456789abcdef".to_string())] {
        let doc = serde_json::json!({"tqs": "3", "id": "t", "kind": "trace", "trace_id": id});
        let _ = query(base, &doc);
        let mut ms = vec![];
        let mut n = 0;
        for _ in 0..10 {
            let (m, p) = query(base, &doc);
            ms.push(m);
            n = p;
        }
        // GETs for one lookup, from DuckDB's HTTP log (S3 only).
        let gets = if s3 {
            let id2 = id.clone();
            node.with_conn(move |c| {
                c.execute_batch("CALL enable_logging('HTTP'); CALL truncate_duckdb_logs();")?;
                let doc = tqs::Tqs::parse(&serde_json::json!({"tqs": "3", "id": "t", "kind": "trace", "trace_id": id2}).to_string())
                    .map_err(|p| anyhow::anyhow!(p.message))?;
                let r = doc.validate(now_ms()).map_err(|p| anyhow::anyhow!("{p:?}"))?;
                c.execute_batch("SET enable_external_file_cache = false; SET parquet_metadata_cache = false;")?;
                tqs::compile::run_list(c, &doc, &r)?;
                let g: i64 =
                    c.query_row("SELECT count(*) FROM duckdb_logs WHERE type = 'HTTP' AND message LIKE '%GET%'", [], |r| r.get(0))?;
                c.execute_batch("CALL disable_logging(); SET enable_external_file_cache = true; SET parquet_metadata_cache = true;")?;
                Ok(g.to_string())
            })
            .await
            .unwrap_or_else(|e| format!("n/a ({e})"))
        } else {
            "—".into()
        };
        rows.push((label, n, pct(&mut ms, 0.5), pct(&mut ms, 0.95), gets));
    }
    writeln!(md, "\n### {title} ({files} span files)\n\n| Trace id | Spans | p50 ms | p95 ms | GETs, cold caches |\n| :--- | ---: | ---: | ---: | ---: |").unwrap();
    for (l, n, p50, p95, g) in rows {
        println!("  trace {l:<8} {files} files  {n} spans  p50 {p50:.1} ms  p95 {p95:.1} ms  GETs {g}");
        writeln!(md, "| {l} | {n} | {p50:.1} | {p95:.1} | {g} |").unwrap();
    }
}

async fn stored_bytes(node: &Arc<Node>, table: &'static str) -> (i64, i64, i64) {
    node.with_conn(move |c| {
        Ok(c.query_row(
            &format!("SELECT count(*), coalesce(sum(data_file_size_bytes), 0), (SELECT count(*) FROM lake.{table}) FROM ducklake_list_files('lake', '{table}')"),
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?)
    })
    .await
    .unwrap()
}

fn main() -> Result<()> {
    let a = args();
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(run(a))
}

async fn run(a: Args) -> Result<()> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let dir = tempfile::tempdir()?;
    let d = dir.path().display();
    let storage = match &a.s3 {
        Some(ep) => format!(
            "storage: {{ url: s3://tiny-eye/bench-{}, s3: {{ endpoint: {ep}, access_key_id: tinyeye, secret_access_key: tinyeye-secret, allow_http: true }} }}",
            ulid::Ulid::generate().to_string().to_lowercase()
        ),
        None => format!("storage: {{ url: {d}/objects }}"),
    };
    let yaml = format!(
        "duckdb: {{ extension_dir: {}, threads: 4, memory_limit: 8GB }}\n{storage}\ncatalog: {{ url: {d}/catalog.ducklake }}\n\
         ingest: {{ wal_dir: {d}/wal, max_sample_age: 30d, flush_interval: 1h, arena_limit: 4GB }}\n\
         coordinator: {{ hot_grace: 1ms, compaction_delay: 1ms, checkpoint_interval: 1h, compaction_interval: 1w, retention_interval: 1w }}\nquery: {{ workers: 4 }}\n",
        root.join(".tools/duckdb_ext").display()
    );
    let cfg = Config::from_layers(Some(&yaml), &[])?;
    let node = Node::open(cfg).await?;
    let state = server::AppState::new(node.clone(), vec![])?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    tokio::spawn(async move { axum::serve(listener, server::app(state)).await });
    let duck_version: String = node.with_conn(|c| Ok(c.query_row("SELECT version()", [], |r| r.get(0))?)).await?;

    let sets = series_set(a.series);
    let steps = a.hours * 3_600_000 / SCRAPE;
    let now = now_ms() / 60_000 * 60_000;
    let t0 = now - a.hours * 3_600_000;
    let live_steps = 600_000 / SCRAPE; // the last 10 minutes arrive later, as live data

    let mut md = String::new();
    writeln!(md, "# tiny-eye benchmark — {}\n", if a.s3.is_some() { "data on S3 (MinIO, same host)" } else { "data on local disk" })?;
    writeln!(
        md,
        "Machine: {} logical CPUs, {}. Engine: DuckDB {duck_version}, 4 DuckDB threads, durability `wal` (fsync per group). \
         HTTP API in process, 4 concurrent remote-write clients.\n\nData: {} series ({} counters, {} gauges, 120 histogram buckets), \
         {} h at a 15 s scrape interval.\n",
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
        std::env::consts::OS,
        sets.len(),
        a.series,
        a.series / 4,
        a.hours
    )?;

    // 1. Backfill
    println!("1. remote-write backfill: {} series × {} steps", sets.len(), steps - live_steps);
    let (samples, secs, mut lat) = backfill(&base, &sets, t0, 0..steps - live_steps, 4, 4);
    let rate = samples as f64 / secs;
    println!("   {samples} samples in {secs:.1} s = {rate:.0} samples/s");
    writeln!(
        md,
        "## Ingest (Prometheus remote-write over HTTP)\n\n| Samples | Seconds | Samples/s | Request p50 ms | p99 ms |\n| ---: | ---: | ---: | ---: | ---: |"
    )?;
    writeln!(md, "| {samples} | {secs:.1} | {rate:.0} | {:.1} | {:.1} |\n", pct(&mut lat, 0.5), pct(&mut lat, 0.99))?;

    // 2. Flush + checkpoint
    let t = Instant::now();
    node.flush_now().await?;
    let flush_s = t.elapsed().as_secs_f64();
    let t = Instant::now();
    node.checkpoint().await?;
    let ckpt_ms = t.elapsed().as_secs_f64() * 1000.0;
    let (l0_files, l0_bytes, rows) = stored_bytes(&node, "metrics_points").await;
    let (_, series_bytes, _) = stored_bytes(&node, "metrics_series").await;
    println!("2. flush {flush_s:.1} s, checkpoint {ckpt_ms:.0} ms, {l0_files} L0 files, {:.2} B/sample", l0_bytes as f64 / rows as f64);
    writeln!(md, "## Write path\n\n| Step | Result |\n| :--- | :--- |")?;
    writeln!(md, "| Flush of the whole backfill (sort, Parquet, PUT, envelope) | {flush_s:.1} s |")?;
    writeln!(md, "| Checkpoint (apply envelope to DuckLake) | {ckpt_ms:.0} ms |")?;
    writeln!(md, "| L0: points files / bytes / bytes per sample | {l0_files} / {l0_bytes} / {:.2} |", l0_bytes as f64 / rows as f64)?;
    writeln!(md, "| Series table bytes | {series_bytes} |")?;

    let (from, to) = (t0 + 600_000, now - live_steps * SCRAPE);
    let mut qmd = String::from("\n## Queries (HTTP round trip, 10 runs after one warm-up)\n");
    println!("3. panels on L0, {} h range", a.hours);
    run_panels(&base, &mut qmd, &format!("{} h range, step 60 s, L0 files", a.hours), from, to, "60s", 10);

    let t = Instant::now();
    let groups = node.compact(now + 2 * 3_600_000).await?;
    let compact_s = t.elapsed().as_secs_f64();
    let (l1_files, l1_bytes, rows1) = stored_bytes(&node, "metrics_points").await;
    println!("   compaction: {groups} groups in {compact_s:.1} s → {l1_files} files, {:.2} B/sample", l1_bytes as f64 / rows1 as f64);
    writeln!(md, "| Compaction (all hours, all tables) | {groups} file groups in {compact_s:.1} s |")?;
    writeln!(md, "| L1: points files / bytes / bytes per sample | {l1_files} / {l1_bytes} / {:.2} |", l1_bytes as f64 / rows1 as f64)?;
    println!("   panels on L1");
    run_panels(&base, &mut qmd, &format!("{} h range, step 60 s, after compaction (L1)", a.hours), from, to, "60s", 10);

    // 4. Live data in the hot tail
    let (live_samples, live_secs, _) = backfill(&base, &sets, t0, steps - live_steps..steps, 1, 4);
    println!("4. live: {live_samples} samples in {live_secs:.1} s stay in RAM; panels over the last hour");
    run_panels(&base, &mut qmd, "last 1 h, step 15 s, last 10 min only in the hot tail", now - 3_600_000, now, "15s", 10);
    md.push_str(&qmd);

    // 5. Traces
    println!("5. traces: {} traces × 6 spans in 11 flushes", a.traces);
    let mut rng = StdRng::seed_from_u64(7);
    let mut ids = vec![];
    let hour_start = (now - 3_600_000) / 3_600_000 * 3_600_000;
    let t = Instant::now();
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
    for f in 0..11 {
        let per = a.traces / 11;
        for chunk in 0..per.div_ceil(500) {
            let n = 500.min(per - chunk * 500);
            let (req, mut got) = traces_request(&mut rng, n, hour_start * 1_000_000, (hour_start + 3_600_000) * 1_000_000);
            ids.append(&mut got);
            let r =
                agent.post(&format!("{base}/v1/traces")).header("Content-Type", "application/x-protobuf").send(&req.encode_to_vec()[..])?;
            assert_eq!(r.status().as_u16(), 200);
        }
        node.flush_and_checkpoint().await?;
        let _ = f;
    }
    let spans_s = t.elapsed().as_secs_f64();
    let (_, span_bytes, span_rows) = stored_bytes(&node, "spans").await;
    println!("   {} spans in {spans_s:.1} s ({:.0} spans/s incl. 11 flushes)", span_rows, span_rows as f64 / spans_s);
    let mut tmd = format!(
        "\n## Traces (OTLP/HTTP protobuf)\n\n{} spans ingested and flushed 11 times in {spans_s:.1} s ({:.0} spans/s); {:.1} bytes per span on L0.\n",
        span_rows,
        span_rows as f64 / spans_s,
        span_bytes as f64 / span_rows as f64
    );
    trace_lookups(&node, &base, &ids, &mut tmd, "Trace lookup by id, 11 L0 files", a.s3.is_some()).await;
    node.compact(now + 2 * 3_600_000).await?;
    trace_lookups(&node, &base, &ids, &mut tmd, "Trace lookup by id, after compaction", a.s3.is_some()).await;
    md.push_str(&tmd);
    md.push_str(CAVEATS);
    md.push_str("\nReproduce: `make bench` (local disk) or `make bench-s3` (MinIO). Numbers are from one run on one machine; MinIO runs on the same host, so S3 latency here is ~1 ms, not a real region's ~20–50 ms.\n");

    if let Some(out) = a.out {
        let p = if std::path::Path::new(&out).is_absolute() { std::path::PathBuf::from(out) } else { root.join(out) };
        std::fs::create_dir_all(p.parent().unwrap())?;
        std::fs::write(&p, &md)?;
        println!("report: {}", p.display());
    } else {
        println!("{md}");
    }
    Ok(())
}
