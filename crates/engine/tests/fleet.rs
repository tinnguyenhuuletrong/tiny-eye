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

fn errors(n: &Node) -> u64 {
    n.metrics.fleet_errors.load(std::sync::atomic::Ordering::Relaxed)
}

/// Metric points `n` queries read: (all, from outside region `us`).
async fn points(n: &Arc<Node>) -> (i64, i64) {
    n.with_conn(|c| {
        Ok(c.query_row("SELECT count(*), count(*) FILTER (WHERE region <> 'us') FROM metrics_points", [], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
        })?)
    })
    .await
    .unwrap()
}

/// A node's published copy and manifest, to publish changed versions of in its place.
struct Copy {
    original: Vec<u8>,
    manifest: serde_json::Value,
    objects: std::path::PathBuf,
    scratch: std::path::PathBuf,
    region: String,
    id: String,
}

impl Copy {
    fn of(shared: &Path, scratch: &Path, region: &str, id: &str) -> Copy {
        let objects = shared.join("objects");
        std::fs::create_dir_all(scratch).unwrap();
        Copy {
            original: std::fs::read(objects.join(fleet::catalog_key(region, id))).unwrap(),
            manifest: serde_json::from_slice(&std::fs::read(objects.join(fleet::manifest_key(id))).unwrap()).unwrap(),
            objects,
            scratch: scratch.to_path_buf(),
            region: region.into(),
            id: id.into(),
        }
    }

    /// `gz` as the copy, and the manifest naming it under snapshot id `snapshot`, published now.
    fn publish_bytes(&self, snapshot: i64, gz: &[u8]) {
        std::fs::write(self.objects.join(fleet::catalog_key(&self.region, &self.id)), gz).unwrap();
        let mut m = self.manifest.clone();
        m["snapshot_id"] = snapshot.into();
        m["published_at_ms"] = now_ms().into();
        std::fs::write(self.objects.join(fleet::manifest_key(&self.id)), serde_json::to_vec(&m).unwrap()).unwrap();
    }

    /// The copy changed by `edit` (on the copy attached as DuckLake `x`, or as the plain metadata
    /// database `r` when `raw`), published under snapshot id `snapshot`.
    fn publish(&self, snapshot: i64, raw: bool, edit: &dyn Fn(&duckdb::Connection)) {
        let settings = engine::duck::DuckSettings {
            extension_dir: concat!(env!("CARGO_MANIFEST_DIR"), "/../../.tools/duckdb_ext").into(),
            threads: 1,
            memory_limit: "512MB".into(),
        };
        let file = self.scratch.join(format!("copy-{snapshot}.ducklake"));
        let mut bytes = vec![];
        std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(&self.original[..]), &mut bytes).unwrap();
        std::fs::write(&file, bytes).unwrap();
        let conn = engine::duck::open(&settings).unwrap();
        let f = file.display();
        if raw {
            conn.execute_batch(&format!("ATTACH '{f}' AS r (TYPE duckdb)")).unwrap();
        } else {
            conn.execute_batch(&format!("ATTACH 'ducklake:{f}' AS x")).unwrap();
        }
        edit(&conn);
        conn.execute_batch(if raw { "DETACH r" } else { "DETACH x" }).unwrap();
        drop(conn);
        self.publish_bytes(snapshot, &gzip(&std::fs::read(&file).unwrap()));
    }

    /// The untouched copy under snapshot id `snapshot`.
    fn restore(&self, snapshot: i64) {
        self.publish_bytes(snapshot, &self.original);
    }
}

fn gzip(b: &[u8]) -> Vec<u8> {
    let mut gz = flate2::write::GzEncoder::new(vec![], flate2::Compression::fast());
    std::io::Write::write_all(&mut gz, b).unwrap();
    gz.finish().unwrap()
}

/// `b` refuses the copy published last: counted once, logged and counted once per snapshot (not
/// downloaded again), nothing of it reaches a query (`own`: what `b` reads alone), and its file is
/// gone. `r`: the test root.
async fn refused(b: &Arc<Node>, own: (i64, i64), r: &Path, what: &str) {
    let before = errors(b);
    assert_eq!(b.refresh_fleet(now_ms()).await.unwrap(), 0, "{what}: refused");
    assert_eq!(errors(b), before + 1, "{what}: counted once");
    assert_eq!(b.refresh_fleet(now_ms()).await.unwrap(), 0, "{what}: still refused");
    assert_eq!(errors(b), before + 1, "{what}: logged and counted once per snapshot");
    assert_eq!(points(b).await, own, "{what}: nothing of the copy reaches a query");
    assert_eq!(files_under(&r.join("node-b/fleet/node-b"), "fleet_"), 0, "{what}: detached and deleted");
}

/// A data file of `node` (under `<objects>/<region>/<node>/metrics_points`).
fn a_data_file(objects: &Path, region: &str, node: &str) -> std::path::PathBuf {
    let mut stack = vec![objects.join(region).join(node).join("metrics_points")];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(d).unwrap().flatten() {
            if e.path().is_dir() {
                stack.push(e.path());
            } else {
                return e.path();
            }
        }
    }
    panic!("no data file of {node}")
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

/// Two processes with one node.id: the one started last publishes; the older one, the stale one,
/// fails every publish (counted). A restarted node is never held back by its predecessor.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn duplicate_node_id_the_newest_process_publishes() {
    let root = tempfile::tempdir().unwrap();
    let shared = root.path().join("shared");
    let manifest_epoch = |shared: &Path| -> i64 {
        let m: Manifest = serde_json::from_slice(&std::fs::read(shared.join("objects").join(fleet::manifest_key("dup"))).unwrap()).unwrap();
        m.epoch
    };
    let open_as = |shared: &Path, dir: &str| Node::open(config(shared, &root.path().join(dir), "dup", "eu", &[]));
    // Older first, then newer: the newer takes over at once, the older stops.
    let x = open_as(&shared, "x").await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let y = open_as(&shared, "y").await.unwrap();
    x.publish().await.unwrap();
    y.publish().await.expect("a newer process overwrites an older one's manifest");
    let e = x.publish().await.expect_err("the older process stops publishing");
    assert!(format!("{e:#}").contains("every node sharing storage.url needs its own node.id"), "{e:#}");
    assert!(x.metrics.render(0).contains("tiny_eye_fleet_errors_total 1\n"), "counted");
    y.publish().await.unwrap();
    assert_eq!(manifest_epoch(&shared), y.epoch);

    // Newer first: the older one never overwrites it.
    let shared2 = root.path().join("shared2");
    let p = open_as(&shared2, "p").await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let q = open_as(&shared2, "q").await.unwrap();
    q.publish().await.unwrap();
    assert!(p.publish().await.is_err(), "an older process does not publish over a newer one, even the first time");
    assert_eq!(manifest_epoch(&shared2), q.epoch);

    // Restart: the new process of y publishes over its predecessor's manifest; x stays muted.
    let old_epoch = y.epoch;
    drop(y);
    let y = open_as(&shared, "y").await.unwrap();
    assert!(y.epoch > old_epoch);
    y.publish().await.expect("a restarted node is not held back by its earlier process");
    assert_eq!(manifest_epoch(&shared), y.epoch);
    assert!(x.publish().await.is_err());

    // A stopped process with a later epoch (its host's clock ran ahead, then stepped back): once
    // its manifest has not been written for the grace, it no longer holds the running one back.
    let shared3 = root.path().join("shared3");
    let z = open_as(&shared3, "z").await.unwrap();
    let file = shared3.join("objects").join(fleet::manifest_key("dup"));
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    let dead = serde_json::json!({"format": fleet::FORMAT, "epoch": z.epoch + 3_600_000});
    std::fs::write(&file, serde_json::to_vec(&dead).unwrap()).unwrap();
    assert!(z.publish().await.is_err(), "a later epoch written just now holds it back");
    let grace = z.cfg.coordinator.snapshot_grace.0;
    std::fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(std::time::SystemTime::now() - grace - std::time::Duration::from_secs(60))
        .unwrap();
    z.publish().await.expect("a manifest not written for the grace is a stopped process's");
    assert_eq!(manifest_epoch(&shared3), z.epoch);
}

/// Copies come from shared storage, so anyone who can write there can write one. A copy that
/// would run code, read files outside its publisher's prefix, does not match this node's tables,
/// is not a catalog at all, or is too large, never reaches a query; nor does a manifest that does
/// not name itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn untrusted_copies_are_refused_before_any_query_reads_them() {
    let root = tempfile::tempdir().unwrap();
    let (shared, r) = (root.path().join("shared"), root.path());
    let a = open(&shared, r, "node-a", "eu", &[]).await;
    let b = open(&shared, r, "node-b", "us", &[]).await;
    let end = end_ms();
    a.ingest(data(&a, "eu", 2, 10, end)).await.unwrap();
    b.ingest(data(&b, "us", 1, 10, end)).await.unwrap();
    for n in [&a, &b] {
        n.flush_and_checkpoint().await.unwrap();
    }
    a.publish().await.unwrap();
    let objects = shared.join("objects");
    let copy = Copy::of(&shared, &r.join("scratch"), "eu", "node-a");
    let own = points(&b).await;
    assert_eq!(own.1, 0);
    // A local file of B's that a copy must never get to read.
    let secret = r.join("secret.txt");
    std::fs::write(&secret, "SECRET-7f3a").unwrap();

    // metrics_points as a DuckLake view that reads a local file (same columns as the table).
    copy.publish(1001, false, &|c| {
        let cols: Vec<(String, String)> = c
            .prepare("SELECT column_name, data_type FROM duckdb_columns() WHERE database_name = 'x' AND table_name = 'metrics_points' ORDER BY column_index")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let select: Vec<String> = cols
            .iter()
            .map(|(n, t)| if t == "VARCHAR" { format!("content AS {n}") } else { format!("CAST(NULL AS {t}) AS {n}") })
            .collect();
        c.execute_batch(&format!(
            "DROP TABLE x.metrics_points; CREATE VIEW x.metrics_points AS SELECT {} FROM read_text('{}')",
            select.join(", "),
            secret.display()
        ))
        .unwrap();
    });
    refused(&b, own, r, "a DuckLake view").await;
    let leaked: i64 = b
        .with_conn(|c| {
            Ok(c.query_row("SELECT count(*) FROM metrics_points WHERE tenant LIKE 'SECRET%' OR region LIKE 'SECRET%'", [], |r| r.get(0))?)
        })
        .await
        .unwrap();
    assert_eq!(leaked, 0);
    // A view in the metadata database itself (DuckLake would run it while reading the catalog).
    copy.publish(1002, true, &|c| {
        c.execute_batch(&format!("CREATE VIEW r.main.ducklake_extra AS SELECT content FROM read_text('{}')", secret.display())).unwrap()
    });
    refused(&b, own, r, "a view in the metadata database").await;
    // Data files outside A's prefix: through `..`, next to it (a prefix of A's prefix is not
    // A's), and in another node's prefix.
    let b_file = a_data_file(&objects, "us", "node-b");
    let canon = std::fs::canonicalize(&objects).unwrap();
    let rel = b_file.strip_prefix(objects.join("us/node-b")).unwrap();
    let sneaky = format!("{}/eu/node-a/../../us/node-b/{}", canon.display(), rel.display());
    let beside = canon.join("eu/node-a-evil").join(rel);
    std::fs::create_dir_all(beside.parent().unwrap()).unwrap();
    std::fs::copy(&b_file, &beside).unwrap();
    let others = canon.join("us/node-b").join(rel);
    for (snapshot, file, what) in [
        (1003, sneaky, "a data file reached through .."),
        (1004, beside.display().to_string(), "a data file in a directory named like A's prefix"),
        (1005, others.display().to_string(), "a data file in another node's prefix"),
    ] {
        copy.publish(snapshot, false, &|c| {
            c.execute_batch(&format!("CALL ducklake_add_data_files('x', 'metrics_points', '{file}')")).unwrap()
        });
        refused(&b, own, r, what).await;
    }
    // Tables that differ from this node's.
    copy.publish(1006, false, &|c| c.execute_batch("CREATE TABLE x.extra (a INTEGER)").unwrap());
    refused(&b, own, r, "an extra table").await;
    // Not a catalog: not gzip, and gzip of something that is not a DuckDB file. Refused for what
    // they hold, so not downloaded again for the same snapshot.
    copy.publish_bytes(1007, b"not gzip at all");
    refused(&b, own, r, "a copy that is not gzip").await;
    copy.publish_bytes(1008, &gzip(&[7u8; 100_000]));
    refused(&b, own, r, "a copy that is not a DuckDB file").await;

    // Too large, for nodes that allow less: once unpacked, and already compressed.
    copy.restore(1009);
    let small = open(&shared, r, "node-s", "us", &[("FLEET__MAX_COPY_BYTES", "64KB")]).await;
    assert!(copy.original.len() < 64 * 1024, "compressed it fits: {} bytes", copy.original.len());
    assert_eq!(small.refresh_fleet(now_ms()).await.unwrap(), 0, "a copy that unpacks to more than fleet.max_copy_bytes");
    assert_eq!(errors(&small), 1);
    let tiny = open(&shared, r, "node-t", "us", &[("FLEET__MAX_COPY_BYTES", "1KB")]).await;
    assert_eq!(tiny.refresh_fleet(now_ms()).await.unwrap(), 0, "a copy larger than fleet.max_copy_bytes compressed");
    assert_eq!(errors(&tiny), 1);

    // The untouched copy is read: the refusals above were about what the copies held.
    assert_eq!(b.refresh_fleet(now_ms()).await.unwrap(), 1, "A's own copy");
    let with_a = points(&b).await;
    assert!(with_a.1 > 0 && with_a.0 == own.0 + with_a.1);

    // Manifests that do not name themselves: node-x points at A's copy; node-y.json is a copy of
    // A's manifest (which would pass every check of the copy, and show A's rows twice).
    let before = errors(&b);
    let m: serde_json::Value = serde_json::from_slice(&std::fs::read(objects.join(fleet::manifest_key("node-a"))).unwrap()).unwrap();
    let mut x = m.clone();
    x["node_id"] = "node-x".into();
    std::fs::write(objects.join(fleet::manifest_key("node-x")), serde_json::to_vec(&x).unwrap()).unwrap();
    std::fs::write(objects.join(fleet::manifest_key("node-y")), serde_json::to_vec(&m).unwrap()).unwrap();
    assert_eq!(b.refresh_fleet(now_ms()).await.unwrap(), 1, "only A: node-x points at A's copy, node-y.json names node-a");
    assert_eq!(errors(&b), before + 1, "one refresh, counted once");
    assert_eq!(points(&b).await, with_a, "A's rows once, not twice");
}

/// DuckLake reads its metadata database by table name, so anything else in it could run when it
/// does: only `ducklake_*` tables in `main` pass, and every inlined-data table the catalog names
/// must be one of them (DuckLake reads a missing one as a file).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_metadata_database_holds_only_ducklake_tables() {
    let root = tempfile::tempdir().unwrap();
    let (shared, r) = (root.path().join("shared"), root.path());
    let a = open(&shared, r, "node-a", "eu", &[]).await;
    let b = open(&shared, r, "node-b", "us", &[]).await;
    let end = end_ms();
    a.ingest(data(&a, "eu", 1, 10, end)).await.unwrap();
    a.flush_and_checkpoint().await.unwrap();
    a.publish().await.unwrap();
    let copy = Copy::of(&shared, &r.join("scratch"), "eu", "node-a");
    let own = points(&b).await;
    let secret = r.join("secret.csv");
    std::fs::write(&secret, "a\nSECRET\n").unwrap();
    let inlined = format!(
        "INSERT INTO r.main.ducklake_inlined_data_tables SELECT table_id, '{}', 1 FROM r.main.ducklake_table WHERE table_name = 'metrics_points'",
        secret.display()
    );
    let cases: Vec<(&str, String)> = vec![
        ("a sequence", "CREATE SEQUENCE r.main.ducklake_seq".into()),
        ("a table not named ducklake_*", "CREATE TABLE r.main.extra (a INTEGER)".into()),
        ("a table in another schema", "CREATE SCHEMA r.s; CREATE TABLE r.s.ducklake_x (a INTEGER)".into()),
        ("another schema", "CREATE SCHEMA r.s".into()),
        ("a type", "CREATE TYPE r.main.ducklake_mood AS ENUM ('a', 'b')".into()),
        ("an index", "CREATE INDEX ducklake_i ON r.main.ducklake_metadata (key)".into()),
        ("a macro", "CREATE MACRO r.main.ducklake_m(v) AS v + 1".into()),
        ("a table macro", "CREATE MACRO r.main.ducklake_tm() AS TABLE SELECT 1 AS a".into()),
        ("a generated column", "CREATE TABLE r.main.ducklake_gen (a INTEGER, b INTEGER GENERATED ALWAYS AS (a + 1))".into()),
        ("an inlined-data table that does not exist", inlined),
    ];
    for (i, (what, sql)) in cases.iter().enumerate() {
        copy.publish(2001 + i as i64, true, &|c| c.execute_batch(sql).unwrap());
        refused(&b, own, r, what).await;
    }
    // A DuckLake macro (DuckLake has no generated columns).
    copy.publish(2100, false, &|c| c.execute_batch("CREATE MACRO x.m(v) AS v + 1").unwrap());
    refused(&b, own, r, "a DuckLake macro").await;
    copy.restore(2101);
    assert_eq!(b.refresh_fleet(now_ms()).await.unwrap(), 1, "the untouched copy passes");
}

/// A manifest is a few hundred bytes: a larger object is not read (a valid manifest with padding
/// would otherwise pass), and the refusal is counted once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversized_manifests_are_not_read() {
    let root = tempfile::tempdir().unwrap();
    let (shared, r) = (root.path().join("shared"), root.path());
    let a = open(&shared, r, "node-a", "eu", &[]).await;
    let b = open(&shared, r, "node-b", "us", &[]).await;
    let mut m = serde_json::to_value(a.publish().await.unwrap()).unwrap();
    m["pad"] = "x".repeat(fleet::MAX_MANIFEST_BYTES as usize).into();
    std::fs::write(shared.join("objects").join(fleet::manifest_key("node-a")), serde_json::to_vec(&m).unwrap()).unwrap();
    assert_eq!(b.refresh_fleet(now_ms()).await.unwrap(), 0, "skipped");
    assert_eq!(errors(&b), 1, "counted");
    assert_eq!(b.refresh_fleet(now_ms()).await.unwrap(), 0);
    assert_eq!(errors(&b), 1, "counted once");
    a.publish().await.unwrap();
    assert_eq!(b.refresh_fleet(now_ms()).await.unwrap(), 1);
}

/// A manifest that cannot be fetched is a passing failure: the node's attached copy stays while it
/// is fresh, and each such refresh is counted.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_manifest_that_cannot_be_read_keeps_a_fresh_copy() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let (shared, r) = (root.path().join("shared"), root.path());
    let a = open(&shared, r, "node-a", "eu", &[]).await;
    let b = open(&shared, r, "node-b", "us", &[]).await;
    let m = a.publish().await.unwrap();
    assert_eq!(b.refresh_fleet(now_ms()).await.unwrap(), 1);
    let file = shared.join("objects").join(fleet::manifest_key("node-a"));
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(&file).is_ok() {
        eprintln!("skipped: permissions do not apply to this user (root)");
        return;
    }
    assert_eq!(b.refresh_fleet(now_ms()).await.unwrap(), 1, "the attached copy is kept");
    assert_eq!(errors(&b), 1, "counted");
    assert_eq!(b.refresh_fleet(now_ms()).await.unwrap(), 1);
    assert_eq!(errors(&b), 2, "counted every time");
    let stale = b.cfg.coordinator.snapshot_grace.ms() - 2 * 30_000 - 10_000;
    assert_eq!(b.refresh_fleet(m.published_at_ms + stale + 1).await.unwrap(), 0, "until it is too old");
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
}

/// A refresh whose listing fails still drops the copies past their stale time, and a publisher's
/// clock running ahead cannot make its copy look younger than storage saw it written.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_listing_still_drops_stale_copies() {
    let root = tempfile::tempdir().unwrap();
    let (shared, r) = (root.path().join("shared"), root.path());
    let a = open(&shared, r, "node-a", "eu", &[]).await;
    let b = open(&shared, r, "node-b", "us", &[]).await;
    let end = end_ms();
    a.ingest(data(&a, "eu", 1, 10, end)).await.unwrap();
    a.flush_and_checkpoint().await.unwrap();
    let m = a.publish().await.unwrap();
    assert_eq!(b.refresh_fleet(now_ms()).await.unwrap(), 1);
    let nodes = |n: &Node| n.metrics.fleet_nodes.load(std::sync::atomic::Ordering::Relaxed);
    // B: grace 10m - 2 × 30s refresh - 10s query timeout.
    let stale = b.cfg.coordinator.snapshot_grace.ms() - 2 * 30_000 - 10_000;
    // A loop under _fleet/ makes the listing fail.
    let fleet_dir = shared.join("objects/_fleet");
    std::os::unix::fs::symlink(".", fleet_dir.join("loop")).unwrap();
    assert!(b.refresh_fleet(m.published_at_ms + stale - 5_000).await.is_err(), "the listing fails");
    assert_eq!((nodes(&b), errors(&b)), (1, 1), "A's copy is still fresh: kept");
    assert!(b.refresh_fleet(m.published_at_ms + stale + 1).await.is_err());
    assert_eq!((nodes(&b), errors(&b)), (0, 2), "past its stale time it is dropped all the same");
    assert_eq!(points(&b).await.1, 0, "no view reads A's copy");
    std::fs::remove_file(fleet_dir.join("loop")).unwrap();

    // A publisher whose clock runs an hour ahead: the manifest's last-modified time counts.
    let mut ahead = serde_json::to_value(&m).unwrap();
    ahead["published_at_ms"] = (now_ms() + 3_600_000).into();
    std::fs::write(fleet_dir.join("node-a.json"), serde_json::to_vec(&ahead).unwrap()).unwrap();
    assert_eq!(b.refresh_fleet(now_ms()).await.unwrap(), 1);
    assert_eq!(b.refresh_fleet(now_ms() + stale + 5_000).await.unwrap(), 0, "stale by the time storage saw it written");
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

/// A copy is read while it is younger than the smaller of the two grace periods minus twice this
/// node's refresh interval and its query timeout: past that the publisher may delete files it
/// names before this node stops reading it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_copy_is_dropped_before_the_smaller_grace_can_run_out() {
    let root = tempfile::tempdir().unwrap();
    let (shared, r) = (root.path().join("shared"), root.path());
    let a = open(&shared, r, "node-a", "eu", &[("COORDINATOR__SNAPSHOT_GRACE", "3m")]).await;
    let b = open(&shared, r, "node-b", "us", &[]).await;
    let c = open(&shared, r, "node-c", "us", &[("COORDINATOR__SNAPSHOT_GRACE", "130s")]).await;
    let m = a.publish().await.unwrap();
    assert_eq!(m.snapshot_grace_ms, 180_000, "the publisher's grace is in the manifest");
    // The time a refresh takes counts too (checked again just before the swap): a little slack
    // on the near side.
    let slack = 3_000;
    // B (grace 10m): A's 3m - 2 × B's 30s refresh - B's 10s query timeout.
    let limit = 180_000 - 2 * 30_000 - 10_000;
    assert_eq!(b.refresh_fleet(m.published_at_ms + limit - slack).await.unwrap(), 1);
    assert_eq!(b.refresh_fleet(m.published_at_ms + limit + 1).await.unwrap(), 0, "past the publisher's grace");
    // C (grace 130s, smaller than A's): C's own grace counts.
    let limit = 130_000 - 2 * 30_000 - 10_000;
    assert_eq!(c.refresh_fleet(m.published_at_ms + limit - slack).await.unwrap(), 1);
    assert_eq!(c.refresh_fleet(m.published_at_ms + limit + 1).await.unwrap(), 0, "past the reader's own grace");
}

/// A stopped node's manifest stays in storage: once it has not been written for the grace, the
/// listing's last-modified time says so and it is not fetched every refresh.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manifests_not_written_for_the_grace_are_not_fetched() {
    let root = tempfile::tempdir().unwrap();
    let (shared, r) = (root.path().join("shared"), root.path());
    let a = open(&shared, r, "node-a", "eu", &[]).await;
    let b = open(&shared, r, "node-b", "us", &[]).await;
    a.publish().await.unwrap();
    let file = shared.join("objects").join(fleet::manifest_key("node-a"));
    let grace = b.cfg.coordinator.snapshot_grace.0;
    // The content says "just published"; only the listing tells the manifest is old.
    let set_mtime = |t: std::time::SystemTime| std::fs::File::options().write(true).open(&file).unwrap().set_modified(t).unwrap();
    set_mtime(std::time::SystemTime::now() - grace - std::time::Duration::from_secs(60));
    assert_eq!(b.refresh_fleet(now_ms()).await.unwrap(), 0, "skipped from the listing alone");
    set_mtime(std::time::SystemTime::now());
    assert_eq!(b.refresh_fleet(now_ms()).await.unwrap(), 1);
}

/// One copy that cannot be read must not keep the views on copies that are stale or gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_broken_copy_never_keeps_the_views_on_stale_or_dropped_copies() {
    let root = tempfile::tempdir().unwrap();
    let (shared, r) = (root.path().join("shared"), root.path());
    let a = open(&shared, r, "node-a", "eu", &[]).await;
    let b = open(&shared, r, "node-b", "us", &[]).await;
    let c = open(&shared, r, "node-c", "asia", &[]).await;
    let end = end_ms();
    for (n, cluster) in [(&a, "eu"), (&b, "us"), (&c, "asia")] {
        n.ingest(data(n, cluster, 1, 10, end)).await.unwrap();
        n.flush_and_checkpoint().await.unwrap();
    }
    a.publish().await.unwrap();
    let c_published = c.publish().await.unwrap().published_at_ms;
    assert_eq!(b.refresh_fleet(now_ms()).await.unwrap(), 2);
    let clusters = |n: Arc<Node>| async move {
        run(&n, &docs(end)[3]).await.into_iter().map(|r| r.1).collect::<std::collections::BTreeSet<_>>().into_iter().collect::<Vec<_>>()
    };
    assert_eq!(clusters(b.clone()).await, ["cluster=asia", "cluster=eu", "cluster=us"]);
    let errors = |n: &Node| n.metrics.fleet_errors.load(std::sync::atomic::Ordering::Relaxed);
    let view_sql = |n: Arc<Node>| async move {
        n.with_conn(|c| {
            Ok(c.query_row("SELECT sql FROM duckdb_views() WHERE database_name = 'memory' AND view_name = 'metrics_points'", [], |r| {
                r.get::<_, String>(0)
            })?)
        })
        .await
        .unwrap()
    };

    // C's alias disappears from under the views (fleet_2: aliases follow node ids). A publishes
    // anew: the swap that still names C's alias fails, and the views are rebuilt with A alone.
    b.with_conn(|c| Ok(c.execute_batch("DETACH fleet_2")?)).await.unwrap();
    a.ingest(data(&a, "eu", 2, 10, end)).await.unwrap();
    a.flush_and_checkpoint().await.unwrap();
    // Well after C's copy, so a time can be found below at which C's is stale and this one not.
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
    let a_published = a.publish().await.unwrap().published_at_ms;
    assert_eq!(b.refresh_fleet(now_ms()).await.unwrap(), 1, "the swap goes on with the copies that answer");
    assert_eq!(errors(&b), 1, "counted once");
    assert!(!view_sql(b.clone()).await.contains("fleet_2"), "no view names the dropped alias");
    assert_eq!(clusters(b.clone()).await, ["cluster=eu", "cluster=us"]);
    assert_eq!(b.refresh_fleet(now_ms()).await.unwrap(), 2, "C's copy is attached again on the next refresh");
    assert_eq!(clusters(b.clone()).await, ["cluster=asia", "cluster=eu", "cluster=us"]);

    // A now publishes a copy of another schema while C has gone quiet: A's copy is refused (its
    // previous one is kept), and C, now stale, leaves the views anyway.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    a.with_conn(|c| Ok(c.execute_batch("ALTER TABLE lake.metrics_points DROP COLUMN is_stale")?)).await.unwrap();
    a.publish().await.unwrap();
    let stale = b.cfg.coordinator.snapshot_grace.ms() - 2 * 30_000 - 10_000;
    let later = a_published + stale - 1_500;
    assert!(later > c_published + stale);
    let c_alias = "fleet_4";
    assert!(view_sql(b.clone()).await.contains(c_alias));
    assert_eq!(b.refresh_fleet(later).await.unwrap(), 1, "A's previous copy; C is stale");
    assert!(!view_sql(b.clone()).await.contains(c_alias), "no view names the stale copy");
    assert_eq!(clusters(b.clone()).await, ["cluster=eu", "cluster=us"]);
}

/// Stored points of a materialized panel never miss accepted data, also when the data arrives at
/// another node: a late sample there for a stored step makes this node recompute that step once
/// the sample's copy is attached.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn late_samples_on_another_node_recompute_materialized_steps() {
    let root = tempfile::tempdir().unwrap();
    let (shared, r) = (root.path().join("shared"), root.path());
    let a = open(&shared, r, "node-a", "eu", &[]).await;
    let b = open(&shared, r, "node-b", "us", &[]).await;
    let now = now_ms() / 15_000 * 15_000;
    a.ingest(data(&a, "eu", 2, 160, now)).await.unwrap();
    b.ingest(data(&b, "us", 1, 160, now)).await.unwrap();
    for n in [&a, &b] {
        n.flush_and_checkpoint().await.unwrap();
    }
    a.publish().await.unwrap();
    assert_eq!(b.refresh_fleet(now_ms()).await.unwrap(), 1);
    let panel = serde_json::json!({"tqs": "3", "id": "p", "kind": "timeseries", "queries": [
        {"name": "r", "metric": "reqs_total", "fn": {"op": "rate", "window": "5m"}, "agg": {"op": "sum", "by": ["job"]}}]});
    b.set_materialized(vec![("t/p".into(), panel.clone())]);
    assert!(b.materialize_since(now - 30 * 60_000, now).await.unwrap() > 0, "steps stored");

    // A receives a sender's backlog: a new job for 5 minutes that B has stored.
    let mut late = Rows::default();
    let labels: Labels = [("job".to_string(), "late".to_string()), ("cluster".to_string(), "eu".to_string())].into();
    let id = engine::ids::series_id(&a.cfg.node.tenant, &a.cfg.node.region, "reqs_total", &labels);
    late.series.push(MetricsSeriesRow {
        series_id: id,
        day_ms: now / 86_400_000 * 86_400_000,
        metric_name: "reqs_total".into(),
        metric_type: 1,
        labels,
        ..Default::default()
    });
    for i in 0..20 {
        late.points.push(MetricsPointsRow {
            series_id: id,
            timestamp_ms: now - 25 * 60_000 + i * 15_000,
            value: (i * 9) as f64,
            ..Default::default()
        });
    }
    a.ingest(late).await.unwrap();
    a.flush_and_checkpoint().await.unwrap();
    a.publish().await.unwrap();
    assert_eq!(b.refresh_fleet(now_ms()).await.unwrap(), 1);
    b.materialize(now).await.unwrap();

    let mut q = panel.clone();
    q["time"] = serde_json::json!({"from": now - 30 * 60_000, "to": now, "step": "15000ms"});
    let shape =
        |o: engine::query::QueryOutput| o.series.into_iter().map(|s| (s.key, s.points)).collect::<std::collections::BTreeMap<_, _>>();
    let mat = b.query_with(&q.to_string(), true).await.unwrap();
    assert!(mat.materialized.as_ref().is_some_and(|i| i.stored_points > 0), "answered from stored points");
    let (mat, raw) = (shape(mat), shape(b.query_with(&q.to_string(), false).await.unwrap()));
    assert!(raw.contains_key("job=late"), "the late job is in the raw answer");
    assert_eq!(mat.keys().collect::<Vec<_>>(), raw.keys().collect::<Vec<_>>());
    for (k, pts) in &raw {
        let got = &mat[k];
        assert_eq!(got.len(), pts.len(), "{k}");
        for ((t, x), (_, y)) in got.iter().zip(pts) {
            assert!((x - y).abs() <= 1e-9 * x.abs().max(y.abs()).max(1.0), "{k} at {t}: stored {x} vs raw {y}");
        }
    }
}

fn rate_panel() -> serde_json::Value {
    serde_json::json!({"tqs": "3", "id": "p", "kind": "timeseries", "queries": [
        {"name": "r", "metric": "reqs_total", "fn": {"op": "rate", "window": "5m"}, "agg": {"op": "sum", "by": ["job"]}}]})
}

/// Restarts recompute only what is new. Another node restarting with the same catalog (a new
/// epoch, no new rows) rewinds nothing; nor does this node restarting, which keeps what its stored
/// points include of each other node next to them (`materialize.path`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restarts_do_not_recompute_stored_points() {
    let root = tempfile::tempdir().unwrap();
    let (shared, r) = (root.path().join("shared"), root.path());
    let mat = r.join("node-b-panels.duckdb");
    let mat_env = [("MATERIALIZE__PATH", mat.to_str().unwrap())];
    let a = open(&shared, r, "node-a", "eu", &[]).await;
    let b = open(&shared, r, "node-b", "us", &mat_env).await;
    let now = now_ms() / 15_000 * 15_000;
    a.ingest(data(&a, "eu", 2, 160, now)).await.unwrap();
    b.ingest(data(&b, "us", 1, 160, now)).await.unwrap();
    for n in [&a, &b] {
        n.flush_and_checkpoint().await.unwrap();
    }
    a.publish().await.unwrap();
    assert_eq!(b.refresh_fleet(now_ms()).await.unwrap(), 1);
    let panels = vec![("t/p".to_string(), rate_panel())];
    b.set_materialized(panels.clone());
    assert!(b.materialize_since(now - 30 * 60_000, now).await.unwrap() > 0, "steps stored");
    assert_eq!(b.materialize(now).await.unwrap(), 0, "caught up");

    // A restarts: a new epoch, the same catalog, nothing new.
    let a_epoch = a.epoch;
    drop(a);
    let a = open(&shared, r, "node-a", "eu", &[]).await;
    assert!(a.epoch > a_epoch);
    a.publish().await.unwrap();
    assert_eq!(b.refresh_fleet(now_ms()).await.unwrap(), 1);
    assert_eq!(b.materialize(now).await.unwrap(), 0, "A's restart shows no new rows: nothing is recomputed");

    // B restarts.
    drop(b);
    let b = open(&shared, r, "node-b", "us", &mat_env).await;
    b.set_materialized(panels.clone());
    assert_eq!(b.refresh_fleet(now_ms()).await.unwrap(), 1);
    assert_eq!(b.materialize(now).await.unwrap(), 0, "B's stored points already include A's copy: nothing is recomputed");
    let stored: i64 = b.with_conn(|c| Ok(c.query_row("SELECT count(*) FROM mat.panel_points", [], |r| r.get(0))?)).await.unwrap();
    assert!(stored > 0);

    // New rows of A for a stored step after both restarts are still noticed.
    let mut late = data(&a, "eu", 1, 4, now - 20 * 60_000);
    late.spans.clear();
    for p in &mut late.points {
        p.value += 1e6;
    }
    a.ingest(late).await.unwrap();
    a.flush_and_checkpoint().await.unwrap();
    a.publish().await.unwrap();
    assert_eq!(b.refresh_fleet(now_ms()).await.unwrap(), 1);
    assert!(b.materialize(now).await.unwrap() > 0, "steps from A's new rows on are recomputed");
}

/// A data file without statistics says nothing about how old its rows are: stored steps are
/// recomputed from the earliest one (no arithmetic on "the oldest possible time").
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_copy_without_file_statistics_recomputes_from_the_earliest_stored_step() {
    let root = tempfile::tempdir().unwrap();
    let (shared, r) = (root.path().join("shared"), root.path());
    let a = open(&shared, r, "node-a", "eu", &[]).await;
    let b = open(&shared, r, "node-b", "us", &[]).await;
    let now = now_ms() / 15_000 * 15_000;
    a.ingest(data(&a, "eu", 2, 160, now)).await.unwrap();
    b.ingest(data(&b, "us", 1, 160, now)).await.unwrap();
    for n in [&a, &b] {
        n.flush_and_checkpoint().await.unwrap();
    }
    b.set_materialized(vec![("t/p".into(), rate_panel())]);
    assert!(b.materialize_since(now - 30 * 60_000, now).await.unwrap() > 0, "steps stored");
    a.publish().await.unwrap();
    let copy = Copy::of(&shared, &r.join("scratch"), "eu", "node-a");
    copy.publish(5001, true, &|c| c.execute_batch("DELETE FROM r.main.ducklake_file_column_stats").unwrap());
    assert_eq!(b.refresh_fleet(now_ms()).await.unwrap(), 1, "a copy without statistics is valid");
    assert!(b.materialize(now).await.expect("the rewind handles a time before every stored step") > 0, "recomputed");
}

/// A catalog written for another object prefix (node.region/node.id changed) is refused with a
/// message that says why and what to do.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_catalog_written_for_another_prefix_fails_clearly() {
    let root = tempfile::tempdir().unwrap();
    let (shared, own) = (root.path().join("shared"), root.path().join("own"));
    drop(Node::open(config(&shared, &own, "a", "eu", &[])).await.unwrap());
    let e = Node::open(config(&shared, &own, "b", "eu", &[])).await.err().expect("another prefix");
    let objects = std::fs::canonicalize(shared.join("objects")).unwrap();
    assert_eq!(
        format!("{e:#}"),
        format!(
            "the catalog {}/catalog.ducklake was written for objects under {o}/eu/a/lake/, but this node writes under {o}/eu/b/lake/ \
             (node.region/node.id changed, or data from a build before the fleet layout). Start with an empty data directory (move \
             ./data aside), or set node.region and node.id back to the values that wrote it.",
            own.display(),
            o = objects.display()
        )
    );
}
