//! The node end to end, in process: ingest → hot tail → flush → envelope → checkpoint →
//! compaction → retention, plus WAL replay after a crash. Local disk, no services.

#![allow(clippy::unwrap_used)]

use engine::node::now_ms;
use engine::tables::*;
use engine::{Config, Node, Rows};
use std::sync::Arc;

/// Test config in `dir`; `env` overrides single keys exactly like `TINY_EYE__*` variables.
fn config(dir: &std::path::Path, env: &[(&str, &str)]) -> Config {
    let ext = concat!(env!("CARGO_MANIFEST_DIR"), "/../../.tools/duckdb_ext");
    let yaml = format!(
        "duckdb: {{ extension_dir: {ext}, threads: 2, memory_limit: 1GB }}\nstorage: {{ url: {d}/objects }}\ncatalog: {{ url: {d}/catalog.ducklake }}\ningest: {{ wal_dir: {d}/wal }}\nmaterialize: {{ path: {d}/mat.duckdb }}\ncoordinator: {{ hot_grace: 1ms, compaction_delay: 1ms }}\n",
        d = dir.display()
    );
    let env: Vec<(String, String)> = env.iter().map(|(k, v)| (format!("TINY_EYE__{k}"), v.to_string())).collect();
    Config::from_layers(Some(&yaml), &env).unwrap()
}

/// `n` samples per series of a counter rising 10 per 15 s scrape, ending now.
fn counter_rows(series: u64, n: i64, end_ms: i64) -> Rows {
    let mut rows = Rows::default();
    for s in 0..series {
        let labels: Labels = [("job".to_string(), format!("j{s}"))].into();
        let id = engine::ids::series_id("default", "local", "reqs_total", &labels);
        rows.series.push(MetricsSeriesRow { series_id: id, metric_name: "reqs_total".into(), labels, ..Default::default() });
        for i in 0..n {
            rows.points.push(MetricsPointsRow {
                series_id: id,
                timestamp_ms: end_ms - (n - 1 - i) * 15_000,
                value: (i * 10) as f64,
                ..Default::default()
            });
        }
    }
    rows
}

/// Instant rate at a fixed `at` (a moving "now" would shift Prometheus' extrapolation).
async fn total_rate(node: &Node, at: i64) -> f64 {
    let q = format!(
        r#"{{"tqs":"3","id":"t","kind":"table","time":{{"from":{},"to":{at}}},"queries":[{{"name":"r","metric":"reqs_total","fn":{{"op":"rate","window":"5m"}},"agg":{{"op":"sum"}}}}]}}"#,
        at - 300_000
    );
    let o = node.query(&q).await.unwrap_or_else(|e| panic!("{e:?}"));
    o.series.first().map(|s| s.points[0].1).unwrap_or(0.0)
}

async fn count(node: &Node, sql: &'static str) -> i64 {
    node.with_conn(move |c| Ok(c.query_row(sql, [], |r| r.get(0))?)).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ingest_flush_checkpoint_compact_retain() {
    let dir = tempfile::tempdir().unwrap();
    let node: Arc<Node> = Node::open(config(dir.path(), &[])).await.unwrap();
    let now = now_ms();
    node.ingest(counter_rows(3, 20, now)).await.unwrap();
    // Prometheus extrapolation caps the start at the counter's zero crossing: 190 / 300 s per series.
    let expect = 3.0 * 190.0 / 300.0;
    let hot = total_rate(&node, now).await;
    assert!((hot - expect).abs() < 0.01, "hot tail only: {hot}");
    let expect = hot;
    let env = node.flush_now().await.unwrap().expect("something to flush");
    assert!(env.files.iter().any(|f| f.table == "metrics_points"));
    assert!((total_rate(&node, now).await - expect).abs() < 1e-9, "flushed, not yet committed: still hot");
    assert_eq!(node.checkpoint().await.unwrap(), 1);
    assert!((total_rate(&node, now).await - expect).abs() < 1e-9, "committed: counted once (lake + hot above watermark)");
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    node.checkpoint().await.unwrap(); // releases hot rows past the grace period
    assert_eq!(count(&node, "SELECT count(*) FROM memory.main.hot_metrics_points").await, 0);
    assert!((total_rate(&node, now).await - expect).abs() < 1e-9, "lake only");
    // A second flush in the same hour, then compaction of hours that are closed.
    node.ingest(counter_rows(3, 1, now + 1)).await.unwrap();
    node.flush_and_checkpoint().await.unwrap();
    let files_before = count(&node, "SELECT count(DISTINCT filename) FROM lake.metrics_points").await;
    assert!(files_before >= 2);
    let compacted = node.compact(now + 2 * 3_600_000).await.unwrap();
    assert!(compacted >= 2, "points + series compacted");
    assert_eq!(count(&node, "SELECT count(*) FROM lake.metrics_points").await, 63);
    assert!(count(&node, "SELECT count(DISTINCT filename) FROM lake.metrics_points").await < files_before);
    assert_eq!(count(&node, "SELECT count(*) FROM lake.metrics_series").await, 3, "series deduplicated by compaction");
    // Retention far in the future drops everything and DuckLake deletes the objects.
    node.retain(now + 30 * 86_400_000).await.unwrap();
    assert_eq!(count(&node, "SELECT count(*) FROM lake.metrics_points").await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wal_replay_after_crash_loses_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let now = now_ms();
    {
        let node = Node::open(config(dir.path(), &[])).await.unwrap();
        node.ingest(counter_rows(2, 10, now)).await.unwrap();
        node.flush_and_checkpoint().await.unwrap(); // first 20 samples reach the lake
        node.ingest(counter_rows(2, 5, now + 15_000 * 5)).await.unwrap(); // acknowledged, never flushed
        node.ingest(counter_rows(1, 3, now)).await.unwrap(); // acknowledged, never flushed
        // "kill -9": drop without flushing.
    }
    let node = Node::open(config(dir.path(), &[])).await.unwrap();
    let pts = count(&node, "SELECT count(*) FROM metrics_points").await;
    assert_eq!(pts, 20 + 10 + 3, "every acknowledged sample is visible after restart");
    node.flush_and_checkpoint().await.unwrap();
    assert_eq!(count(&node, "SELECT count(*) FROM lake.metrics_points").await, 33);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn arena_limit_refuses_with_overload() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::open(config(dir.path(), &[("INGEST__ARENA_LIMIT", "10KB")])).await.unwrap();
    let r = node.ingest(counter_rows(50, 50, now_ms())).await;
    assert!(matches!(r, Err(engine::IngestError::Overloaded)), "{r:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spans_and_logs_round_trip_through_the_lake() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::open(config(dir.path(), &[])).await.unwrap();
    let now_ns = now_ms() * 1_000_000;
    let mut rows = Rows::default();
    for i in 0..10u64 {
        rows.spans.push(SpansRow {
            trace_id: [7; 16],
            span_id: i + 1,
            parent_span_id: (i > 0).then_some(1),
            start_time_ns: now_ns - 1_000_000_000 + i as i64 * 1_000_000,
            duration_ns: 5_000_000,
            service_name: "api".into(),
            span_name: format!("op{}", i % 2),
            status_code: if i == 3 { 2 } else { 0 },
            ..Default::default()
        });
        rows.logs.push(LogsRow {
            timestamp_ns: now_ns - 1_000_000_000,
            service_name: "api".into(),
            severity_number: 17,
            severity_text: "ERROR".into(),
            body: format!("timeout after {i}ms"),
            pattern_hash: engine::ids::pattern_hash(&format!("timeout after {i}ms")),
            trace_id: Some([7; 16]),
            ..Default::default()
        });
    }
    node.ingest(rows).await.unwrap();
    node.flush_and_checkpoint().await.unwrap();
    let w = node.query(&format!(r#"{{"tqs":"3","id":"w","kind":"trace","trace_id":"{}"}}"#, "07".repeat(16))).await.unwrap();
    assert_eq!(w.rows.len(), 10);
    let e = node
        .query(r#"{"tqs":"3","id":"e","kind":"table","time":{"from":"-1m"},"queries":[{"name":"e","spans":[{"field":"status_code","op":"=","value":2}],"measure":{"op":"count"}}]}"#)
        .await
        .unwrap();
    assert_eq!(e.series[0].points[0].1, 1.0);
    // Grouping by a resource attribute binds the key (review finding 6).
    let g = node
        .query(r#"{"tqs":"3","id":"g","kind":"table","time":{"from":"-1m"},"queries":[{"name":"n","spans":[],"measure":{"op":"count"},"agg":{"by":["resource.k8s.pod"]}}]}"#)
        .await
        .unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(g.series[0].points[0].1, 10.0);
    // `exists` on an attribute (review: it bound the key twice and failed).
    let ex = node
        .query(r#"{"tqs":"3","id":"x","kind":"log_search","time":{"from":"-1m"},"search":{"logs":[{"field":"attr.logger","op":"exists","value":false}]}}"#)
        .await
        .unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(ex.rows.len(), 10, "no log row has a logger attribute");
    let p = node.query(r#"{"tqs":"3","id":"p","kind":"log_patterns","time":{"from":"-1m"}}"#).await.unwrap();
    assert_eq!(p.rows.len(), 1, "10 lines, one template");
    let c = node.catalog().await.unwrap();
    assert_eq!(c.services[0].name, "api");
}

/// Review finding 1: a flush that fails must be retried before anything newer, so the rows it
/// held are never hidden by a later commit or released from RAM.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_flush_is_retried_and_loses_nothing() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let node = Node::open(config(dir.path(), &[])).await.unwrap();
    let now = now_ms();
    node.ingest(counter_rows(2, 10, now)).await.unwrap(); // A: 20 samples
    let objects = dir.path().join("objects");
    std::fs::set_permissions(&objects, std::fs::Permissions::from_mode(0o555)).unwrap();
    assert!(node.flush_now().await.is_err(), "flush must fail on a read-only store");
    std::fs::set_permissions(&objects, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(count(&node, "SELECT count(*) FROM metrics_points").await, 20, "A stays visible from RAM");
    let mut b = counter_rows(1, 5, now);
    for r in b.series.iter_mut() {
        r.labels.insert("job".into(), "late".into());
        r.series_id = engine::ids::series_id("default", "local", "reqs_total", &r.labels);
    }
    let id = b.series[0].series_id;
    for p in b.points.iter_mut() {
        p.series_id = id;
    }
    node.ingest(b).await.unwrap(); // B: 5 samples
    node.flush_and_checkpoint().await.unwrap(); // writes A, then B
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    node.checkpoint().await.unwrap(); // releases hot rows
    assert_eq!(count(&node, "SELECT count(*) FROM memory.main.hot_metrics_points").await, 0);
    assert_eq!(count(&node, "SELECT count(*) FROM lake.metrics_points").await, 25, "A and B both reached the lake");
}

/// Review finding 2: queries abandoned by their caller (client disconnect) must give their
/// connection back to the pool.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn abandoned_queries_return_their_connection() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::open(config(dir.path(), &[("QUERY__WORKERS", "2")])).await.unwrap();
    for _ in 0..4 {
        let n = node.clone();
        let h = tokio::spawn(async move {
            n.with_conn(|_| {
                std::thread::sleep(std::time::Duration::from_millis(100));
                Ok(())
            })
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        h.abort(); // the HTTP handler future is dropped
    }
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    for _ in 0..4 {
        assert_eq!(count(&node, "SELECT 1").await, 1, "pool still has connections");
    }
}

/// A statement that runs for about a minute (2 DuckDB threads) unless interrupted; bounded, so
/// a regression fails the test instead of hanging it (runtime shutdown waits for blocking jobs).
const ENDLESS: &str = "SELECT count(*) FROM range(30000000000) a WHERE a.range % 7 = 3";

/// Review finding: a query whose caller went away is interrupted at once, so it cannot run
/// unbounded and hold its connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn abandoned_queries_are_interrupted() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::open(config(dir.path(), &[("QUERY__WORKERS", "1"), ("QUERY__TIMEOUT", "1h")])).await.unwrap();
    let n = node.clone();
    let h = tokio::spawn(async move { n.with_conn(|c| Ok(c.query_row(ENDLESS, [], |r| r.get::<_, i64>(0))?)).await });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    h.abort(); // the HTTP client disconnects
    let t = std::time::Instant::now();
    let next = tokio::time::timeout(std::time::Duration::from_secs(10), count(&node, "SELECT 1")).await;
    assert_eq!(next.ok(), Some(1), "the only connection is free again");
    assert!(t.elapsed() < std::time::Duration::from_secs(5), "{:?}", t.elapsed());
}

/// Review finding: waiting for a connection and running share one `query.timeout`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pool_wait_and_run_share_one_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::open(config(dir.path(), &[("QUERY__WORKERS", "1"), ("QUERY__TIMEOUT", "1500ms")])).await.unwrap();
    let n = node.clone();
    let holder = tokio::spawn(async move {
        n.with_conn(|_| {
            std::thread::sleep(std::time::Duration::from_millis(1000));
            Ok(())
        })
        .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let t = std::time::Instant::now();
    let r = node.with_conn(|c| Ok(c.query_row(ENDLESS, [], |r| r.get::<_, i64>(0))?)).await;
    let took = t.elapsed();
    assert!(format!("{:#}", r.unwrap_err()).contains("exceeded the 1500 ms timeout"));
    holder.await.unwrap().unwrap();
    // ~1450 ms with one deadline; ~2450 ms if the run got a fresh 1500 ms after the wait.
    assert!(took < std::time::Duration::from_millis(2000), "{took:?}");
}

/// Review findings 5 and 7: a retried request stores each sample once, and a refused request
/// does not hide its series row from the retry.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retries_do_not_double_count_or_lose_series() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::open(config(dir.path(), &[("INGEST__ARENA_LIMIT", "300KB")])).await.unwrap();
    let now = now_ms();
    assert!(matches!(node.ingest(counter_rows(300, 100, now)).await, Err(engine::IngestError::Overloaded)));
    node.ingest(counter_rows(2, 10, now)).await.unwrap();
    node.ingest(counter_rows(2, 10, now)).await.unwrap(); // the same request again (lost response)
    node.flush_and_checkpoint().await.unwrap();
    assert_eq!(count(&node, "SELECT count(*) FROM lake.metrics_points").await, 20, "duplicates collapse at flush");
    assert_eq!(count(&node, "SELECT count(*) FROM metrics_series").await, 2, "series rows survive the refused request");
}
