//! A fleet in process: nodes with their own node.id, region and catalog file share one storage
//! directory, publish catalog copies and query each other's data (see `fleet.rs`). Local disk,
//! no services.

#![allow(clippy::unwrap_used)]

use engine::fleet::{self, Manifest};
use engine::node::now_ms;
use engine::tables::*;
use engine::{Config, Node, Rows};
use std::path::Path;
use std::sync::Arc;

/// A node `id` in `region` whose objects go to `<shared>/objects` and local files to `own`.
fn config(shared: &Path, own: &Path, id: &str, region: &str, env: &[(&str, &str)]) -> Config {
    let ext = concat!(env!("CARGO_MANIFEST_DIR"), "/../../.tools/duckdb_ext");
    let yaml = format!(
        "node: {{ id: {id}, region: {region} }}\nduckdb: {{ extension_dir: {ext}, threads: 2, memory_limit: 1GB }}\n\
         storage: {{ url: {s}/objects }}\ncatalog: {{ url: {d}/catalog.ducklake }}\ningest: {{ wal_dir: {d}/wal }}\n\
         fleet: {{ dir: {d}/fleet }}\nmaterialize: {{ path: '' }}\ncoordinator: {{ hot_grace: 1ms, compaction_delay: 1ms }}\n\
         query: {{ workers: 2, timeout: 10s }}\n",
        s = shared.display(),
        d = own.display()
    );
    let env: Vec<(String, String)> = env.iter().map(|(k, v)| (format!("TINY_EYE__{k}"), v.to_string())).collect();
    Config::from_layers(Some(&yaml), &env).unwrap()
}

async fn open(shared: &Path, root: &Path, id: &str, region: &str, env: &[(&str, &str)]) -> Arc<Node> {
    Node::open(config(shared, &root.join(id), id, region, env)).await.unwrap()
}

/// The sender's samples as `node` receives them: series ids use the receiving node's region.
/// A counter rising 10 per 15 s scrape, a gauge, and one span per scrape, per job, labelled
/// `cluster=<cluster>` like Prometheus external labels.
fn data(node: &Node, cluster: &str, jobs: usize, n: i64, end: i64) -> Rows {
    let (tenant, region) = (&node.cfg.node.tenant, &node.cfg.node.region);
    let mut rows = Rows::default();
    for j in 0..jobs {
        for (metric, mtype) in [("reqs_total", 1u8), ("mem_bytes", 2)] {
            let labels: Labels = [("job".to_string(), format!("j{j}")), ("cluster".to_string(), cluster.to_string())].into();
            let id = engine::ids::series_id(tenant, region, metric, &labels);
            let day_ms = end / 86_400_000 * 86_400_000;
            rows.series.push(MetricsSeriesRow {
                series_id: id,
                day_ms,
                metric_name: metric.into(),
                metric_type: mtype,
                labels,
                ..Default::default()
            });
            for i in 0..n {
                let value = if mtype == 1 { (i * 10 + j as i64) as f64 } else { 1e6 + ((i * 7 + j as i64 * 3) % 11) as f64 };
                rows.points.push(MetricsPointsRow { series_id: id, timestamp_ms: end - (n - 1 - i) * 15_000, value, ..Default::default() });
            }
        }
        for i in 0..n {
            rows.spans.push(SpansRow {
                trace_id: {
                    let mut t = [j as u8 + 1; 16];
                    t[15] = cluster.as_bytes()[0];
                    t
                },
                span_id: (end - (n - 1 - i) * 15_000) as u64, // unique per sample time: compaction keeps one row per (trace, span)
                start_time_ns: (end - (n - 1 - i) * 15_000) * 1_000_000,
                duration_ns: 1_000_000 * (i as u64 + 1),
                service_name: format!("svc-{cluster}"),
                span_name: format!("op{j}"),
                ..Default::default()
            });
        }
    }
    rows
}

fn docs(end: i64) -> Vec<String> {
    let t = serde_json::json!({"from": end - 9 * 60_000, "to": end, "step": "60s"});
    let doc = |id: &str, kind: &str, q: serde_json::Value| {
        serde_json::json!({"tqs": "3", "id": id, "kind": kind, "time": t, "queries": q}).to_string()
    };
    vec![
        doc(
            "rate",
            "timeseries",
            serde_json::json!([{"name": "r", "metric": "reqs_total", "fn": {"op": "rate", "window": "5m"}, "agg": {"op": "sum", "by": ["job"]}}]),
        ),
        doc("per_series", "timeseries", serde_json::json!([{"name": "r", "metric": "reqs_total", "fn": {"op": "rate", "window": "5m"}}])),
        doc(
            "gauge",
            "timeseries",
            serde_json::json!([{"name": "m", "metric": "mem_bytes", "fn": {"op": "last"}, "agg": {"op": "max", "by": ["cluster"]}}]),
        ),
        doc(
            "table",
            "table",
            serde_json::json!([{"name": "i", "metric": "reqs_total", "fn": {"op": "increase", "window": "5m"}, "agg": {"op": "sum", "by": ["cluster"]}}]),
        ),
        doc("spans", "table", serde_json::json!([{"name": "n", "spans": [], "measure": {"op": "count"}, "agg": {"by": ["service_name"]}}])),
        serde_json::json!({"tqs": "3", "id": "trace", "kind": "trace", "trace_id": format!("{}65", "01".repeat(15))}).to_string(),
    ]
}

type Out = Vec<(String, String, i64, f64)>;

/// Sorted result points of a document (list kinds: rows without `region`, which names the source).
async fn run(node: &Node, json: &str) -> Out {
    let o = node.query_with(json, false).await.unwrap_or_else(|e| panic!("{e:?}"));
    let mut out: Out =
        o.series.into_iter().flat_map(|s| s.points.into_iter().map(move |(t, v)| (s.output.clone(), s.key.clone(), t, v))).collect();
    for mut r in o.rows {
        r.remove("region");
        out.push((format!("{r:?}"), String::new(), 0, 0.0));
    }
    out.sort_by(|a, b| (&a.0, &a.1, a.2).cmp(&(&b.0, &b.1, b.2)));
    out
}

async fn run_all(node: &Node, end: i64) -> Vec<Out> {
    let mut v = vec![];
    for d in docs(end) {
        v.push(run(node, &d).await);
    }
    v
}

fn equal(a: &Out, b: &Out) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| x.0 == y.0 && x.1 == y.1 && x.2 == y.2 && (x.3 - y.3).abs() <= 1e-9 * x.3.abs().max(y.3.abs()).max(1.0))
}

fn assert_all_equal(got: &[Out], want: &[Out], what: &str) {
    for (i, (g, w)) in got.iter().zip(want).enumerate() {
        // The trace lookup (last) names an eu trace; a node without eu data finds none.
        assert!(!w.is_empty() || i + 1 == want.len(), "{what}: query {i} returned nothing to compare");
        assert!(equal(g, w), "{what}: query {i} differs\n got {g:?}\nwant {w:?}");
    }
}

fn end_ms() -> i64 {
    now_ms() / 15_000 * 15_000 - 15_000
}

fn files_under(dir: &Path, prefix: &str) -> usize {
    fn walk(d: &Path, prefix: &str, n: &mut usize) {
        for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, prefix, n);
            } else if p.file_name().and_then(|f| f.to_str()).is_some_and(|f| f.starts_with(prefix)) {
                *n += 1;
            }
        }
    }
    let mut n = 0;
    walk(dir, prefix, &mut n);
    n
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fleet_answers_like_one_node_that_received_everything() {
    let root = tempfile::tempdir().unwrap();
    let (shared, r) = (root.path().join("shared"), root.path());
    let a = open(&shared, r, "node-a", "eu", &[]).await;
    let b = open(&shared, r, "node-b", "us", &[]).await;
    let base = open(&r.join("base-shared"), r, "base", "all", &[]).await;
    let end = end_ms();
    a.ingest(data(&a, "eu", 3, 40, end)).await.unwrap();
    b.ingest(data(&b, "us", 2, 40, end)).await.unwrap();
    base.ingest(data(&base, "eu", 3, 40, end)).await.unwrap();
    base.ingest(data(&base, "us", 2, 40, end)).await.unwrap();
    for n in [&a, &b, &base] {
        n.flush_and_checkpoint().await.unwrap();
    }
    let alone = run_all(&b, end).await;
    assert!(alone[3].iter().all(|r| r.1 == "cluster=us"), "before refreshing B reads only its own data: {:?}", alone[3]);

    a.publish().await.unwrap();
    b.publish().await.unwrap();
    assert_eq!(b.refresh_fleet(now_ms()).await.unwrap(), 1, "B finds A, not itself");
    assert_eq!(a.refresh_fleet(now_ms()).await.unwrap(), 1, "A finds B");
    let want = run_all(&base, end).await;
    assert_all_equal(&run_all(&b, end).await, &want, "node B over A + B vs one node");
    assert_all_equal(&run_all(&a, end).await, &want, "node A over A + B vs one node");

    // freshness_s: the age of the oldest other-node copy; self-metrics say the same.
    let o = b.query(&docs(end)[0]).await.unwrap();
    assert!(o.freshness_s > 0.0 && o.freshness_s < 60.0, "freshness {}", o.freshness_s);
    let m = b.metrics.render(0);
    assert!(m.contains("tiny_eye_fleet_nodes 1\n") && m.contains("tiny_eye_fleet_oldest_copy_age_seconds "), "{m}");

    // Object layout: each node under <region>/<id>/, manifests at the shared root.
    let objects = shared.join("objects");
    let mut top: Vec<String> = std::fs::read_dir(&objects).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    top.sort();
    assert_eq!(top, ["_fleet", "eu", "us"], "nothing outside the node prefixes but the manifests");
    for (region, id) in [("eu", "node-a"), ("us", "node-b")] {
        let own = objects.join(region).join(id);
        assert!(files_under(&own.join("metrics_points"), "l0-") > 0, "{id}: data under {region}/{id}/");
        assert!(own.join(fleet::CATALOG_KEY).is_file(), "{id}: catalog copy under {region}/{id}/");
        let m: Manifest = serde_json::from_slice(&std::fs::read(objects.join(fleet::manifest_key(id))).unwrap()).unwrap();
        assert_eq!((m.node_id.as_str(), m.region.as_str(), m.format), (id, region, fleet::FORMAT));
        assert_eq!(m.catalog_key, format!("{region}/{id}/_fleet/catalog.ducklake.gz"));
        assert!(m.snapshot_id > 0 && m.published_at_ms > 0);
    }
    let base_top: Vec<String> =
        std::fs::read_dir(r.join("base-shared/objects")).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    assert_eq!(base_top, ["all"], "a node that never published writes only under <region>/");
    assert!(files_under(&r.join("base-shared/objects/all/base"), "l0-") > 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn other_nodes_rows_show_after_publish_and_refresh_own_rows_at_once() {
    let root = tempfile::tempdir().unwrap();
    let (shared, r) = (root.path().join("shared"), root.path());
    let env = [("QUERY__TIMEOUT", "3s")];
    let a = open(&shared, r, "node-a", "eu", &env).await;
    let b = open(&shared, r, "node-b", "us", &env).await;
    a.publish().await.unwrap();
    b.refresh_fleet(now_ms()).await.unwrap();
    let end = end_ms();
    let count = |cluster: &'static str| {
        let b = b.clone();
        async move { run(&b, &docs(end)[3]).await.iter().filter(|r| r.1 == format!("cluster={cluster}")).count() }
    };
    b.ingest(data(&b, "us", 1, 5, end)).await.unwrap();
    assert_eq!(count("us").await, 1, "own rows: visible once acknowledged (hot tail)");
    a.ingest(data(&a, "eu", 1, 5, end)).await.unwrap();
    assert_eq!(count("eu").await, 0, "A's rows: only in A's hot tail");
    a.flush_and_checkpoint().await.unwrap();
    assert_eq!(count("eu").await, 0, "in A's catalog, not yet published");
    a.publish().await.unwrap();
    assert_eq!(count("eu").await, 0, "published, B has not refreshed");
    b.refresh_fleet(now_ms()).await.unwrap();
    assert_eq!(count("eu").await, 1, "after B's refresh");
    assert_eq!(count("us").await, 1);

    // A replaced copy stays attached until no query that started before the swap can run.
    let copies = || files_under(&r.join("node-b/fleet/node-b"), "fleet_");
    assert_eq!(copies(), 2, "A's first (empty) copy is kept for query.timeout after the swap");
    a.ingest(data(&a, "eu", 2, 5, end)).await.unwrap();
    a.flush_and_checkpoint().await.unwrap();
    a.publish().await.unwrap();
    b.refresh_fleet(now_ms()).await.unwrap();
    assert_eq!(copies(), 3, "replaced copies are kept for query.timeout");
    assert_eq!(count("eu").await, 1);
    tokio::time::sleep(std::time::Duration::from_millis(3100)).await;
    b.refresh_fleet(now_ms()).await.unwrap();
    assert_eq!(copies(), 1, "then detached and deleted");
    let jobs = run(&b, &docs(end)[0]).await.iter().map(|r| r.1.clone()).collect::<std::collections::BTreeSet<_>>();
    assert_eq!(jobs.len(), 2, "j0 (both clusters) and j1 (eu): {jobs:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_alone_is_unchanged() {
    let root = tempfile::tempdir().unwrap();
    let (shared, r) = (root.path().join("shared"), root.path());
    let n = open(&shared, r, "node-0", "local", &[]).await;
    let views = |n: Arc<Node>| async move {
        n.with_conn(|c| {
            let mut s = c.prepare("SELECT view_name, sql FROM duckdb_views() WHERE database_name = 'memory' ORDER BY 1")?;
            Ok(s.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .unwrap()
    };
    let before = views(n.clone()).await;
    assert_eq!(before.len(), Table::ALL.len());
    let end = end_ms();
    n.ingest(data(&n, "eu", 2, 20, end)).await.unwrap();
    n.flush_and_checkpoint().await.unwrap();
    let want = run_all(&n, end).await;
    n.publish().await.unwrap();
    assert_eq!(n.refresh_fleet(now_ms()).await.unwrap(), 0, "a node finds only itself");
    assert_eq!(views(n.clone()).await, before, "the views are the single-node views");
    assert_all_equal(&run_all(&n, end).await, &want, "alone, after publish + refresh");
    assert_eq!(n.query(&docs(end)[0]).await.unwrap().freshness_s, 0.0);
    assert!(n.metrics.render(0).contains("tiny_eye_fleet_nodes 0\n"));
    assert!(shared.join("objects/local/node-0/metrics_points").is_dir(), "default region and id: objects under local/node-0/");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_on_one_node_keeps_copies_readable_within_the_grace() {
    let root = tempfile::tempdir().unwrap();
    let (shared, r) = (root.path().join("shared"), root.path());
    let a = open(&shared, r, "node-a", "eu", &[]).await;
    let b = open(&shared, r, "node-b", "us", &[]).await;
    let grace = a.cfg.coordinator.snapshot_grace.ms();
    let end = end_ms();
    // Two flushes: two L0 files per table and hour on A, which compaction replaces.
    a.ingest(data(&a, "eu", 2, 20, end - 20 * 15_000)).await.unwrap();
    a.flush_and_checkpoint().await.unwrap();
    a.ingest(data(&a, "eu", 2, 20, end)).await.unwrap();
    a.flush_and_checkpoint().await.unwrap();
    b.ingest(data(&b, "us", 1, 20, end)).await.unwrap();
    b.flush_and_checkpoint().await.unwrap();
    let b_alone = run_all(&b, end).await;
    a.publish().await.unwrap();
    assert_eq!(b.refresh_fleet(now_ms()).await.unwrap(), 1);
    let fleet = run_all(&b, end).await;
    let a_alone = run_all(&a, end).await;
    let a_points = shared.join("objects/eu/node-a/metrics_points");
    let l0 = files_under(&a_points, "l0-");
    assert!(l0 >= 2);

    // A compacts: B's copy still names the L0 files. A snapshot that was current until the
    // compaction is kept for the grace after it, however old it is.
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    let compacted_at = now_ms();
    assert!(a.compact(end + 2 * 3_600_000).await.unwrap() > 0);
    let compacted_end = now_ms();
    assert_eq!(files_under(&a_points, "l0-"), l0, "compaction alone deletes nothing");
    a.retain(compacted_at + grace - 600).await.unwrap();
    assert_eq!(files_under(&a_points, "l0-"), l0, "within the grace the files B's copy names are kept");
    assert_all_equal(&run_all(&b, end).await, &fleet, "B over its old copy of A, after A's compaction and retention");

    // Past the grace they go, and B skips A's manifest, which is now older than the grace.
    let later = compacted_end + grace + 1000;
    a.retain(later).await.unwrap();
    assert_eq!(files_under(&a_points, "l0-"), 0, "past the grace the replaced files are deleted");
    assert_all_equal(&run_all(&a, end).await, &a_alone, "A itself after compaction and retention");
    assert_eq!(b.refresh_fleet(later).await.unwrap(), 0, "a stale manifest is skipped");
    assert_all_equal(&run_all(&b, end).await, &b_alone, "B without A");
    // A publishes again: B reads A's compacted files, same answers.
    a.publish().await.unwrap();
    assert_eq!(b.refresh_fleet(now_ms()).await.unwrap(), 1);
    assert_all_equal(&run_all(&b, end).await, &fleet, "B over A's compacted data");
}

/// The stated rule for identical label sets: series ids include the receiving node's region, so
/// one label set sent to nodes in two regions is two series (aggregations add them up), and sent
/// to two nodes of one region it is one series whose samples merge, as on one node. No region
/// label is added: senders label their region, like Prometheus external labels.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn identical_label_sets_on_two_nodes() {
    let root = tempfile::tempdir().unwrap();
    let (shared, r) = (root.path().join("shared"), root.path());
    let a = open(&shared, r, "node-a", "eu", &[]).await;
    let b = open(&shared, r, "node-b", "us", &[]).await;
    let c = open(&shared, r, "node-c", "eu", &[]).await;
    let end = end_ms();
    let temp = |n: &Node, value: f64, shift: i64| {
        let labels: Labels = [("room".to_string(), "x".to_string())].into();
        let id = engine::ids::series_id(&n.cfg.node.tenant, &n.cfg.node.region, "temp", &labels);
        let mut rows = Rows::default();
        let day_ms = end / 86_400_000 * 86_400_000;
        rows.series.push(MetricsSeriesRow {
            series_id: id,
            day_ms,
            metric_name: "temp".into(),
            metric_type: 2,
            labels,
            ..Default::default()
        });
        for i in 0..8 {
            rows.points.push(MetricsPointsRow { series_id: id, timestamp_ms: end - shift - i * 15_000, value, ..Default::default() });
        }
        rows
    };
    a.ingest(temp(&a, 1.0, 10_000)).await.unwrap();
    b.ingest(temp(&b, 2.0, 0)).await.unwrap();
    c.ingest(temp(&c, 4.0, 5_000)).await.unwrap(); // same series id as A's; its samples are newer
    for n in [&a, &b, &c] {
        n.flush_and_checkpoint().await.unwrap();
        n.publish().await.unwrap();
    }
    assert_eq!(b.refresh_fleet(now_ms()).await.unwrap(), 2);
    let cat = b.catalog().await.unwrap();
    let t = cat.metrics.iter().find(|m| m.name == "temp").unwrap();
    assert_eq!(t.series, 2, "one series per region: eu (A and C merged) and us");
    let q = |agg: &str| {
        format!(
            r#"{{"tqs":"3","id":"t","kind":"table","time":{{"from":{},"to":{end}}},"queries":[{{"name":"t","metric":"temp","fn":{{"op":"last"}}{agg}}}]}}"#,
            end - 60_000
        )
    };
    let sum = run(&b, &q(r#","agg":{"op":"sum"}"#)).await;
    assert_eq!(sum.len(), 1);
    assert!((sum[0].3 - 6.0).abs() < 1e-9, "us 2 + eu latest sample (C's 4): {sum:?}");
    // Without aggregation results are keyed by labels: the two series share one key, which
    // holds one of their values. A label that tells the senders apart (cluster, region) avoids it.
    let raw = run(&b, &q("")).await;
    assert_eq!(raw.len(), 1, "one result per label set: {raw:?}");
    assert!(raw[0].1 == "room=x" && (raw[0].3 == 2.0 || raw[0].3 == 4.0), "{raw:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_process_with_the_same_node_id_is_detected() {
    let root = tempfile::tempdir().unwrap();
    let shared = root.path().join("shared");
    let x = Node::open(config(&shared, &root.path().join("x"), "dup", "eu", &[])).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let y = Node::open(config(&shared, &root.path().join("y"), "dup", "us", &[])).await.unwrap();
    x.publish().await.unwrap();
    y.publish().await.unwrap();
    let e = x.publish().await.expect_err("y overwrote x's manifest");
    assert!(format!("{e:#}").contains("needs its own node.id"), "{e:#}");
    y.publish().await.unwrap();
}

/// Spec Q19: publishing copies DuckLake's internal metadata database by name. A DuckDB or
/// DuckLake upgrade that renames it fails here (and in CI) before it fails a fleet.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ducklake_metadata_database_keeps_its_name() {
    let root = tempfile::tempdir().unwrap();
    let n = open(&root.path().join("shared"), root.path(), "node-0", "local", &[]).await;
    let found = n
        .with_conn(|c| {
            Ok(c.query_row(&format!("SELECT count(*) FROM {}.ducklake_snapshot", fleet::METADATA_DB), [], |r| r.get::<_, i64>(0))?)
        })
        .await;
    assert!(
        found.as_ref().is_ok_and(|n| *n > 0),
        "{} is gone ({found:?}): update engine::fleet::METADATA_DB for this DuckLake version",
        fleet::METADATA_DB
    );
    let m = n.publish().await.unwrap();
    assert!(root.path().join("shared/objects").join(&m.catalog_key).is_file());
}
