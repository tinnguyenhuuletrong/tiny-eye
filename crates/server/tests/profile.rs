//! Not a test of behaviour: `cargo test --release -p server --test profile -- --ignored --nocapture`
//! prints DuckDB's EXPLAIN ANALYZE for a heavy panel on generated data, to find where time goes.

#![allow(clippy::unwrap_used)]

use engine::node::now_ms;
use engine::tables::*;
use engine::{Config, Node, Rows};

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn explain_heavy_panel() {
    let dir = tempfile::tempdir().unwrap();
    let ext = concat!(env!("CARGO_MANIFEST_DIR"), "/../../.tools/duckdb_ext");
    let d = dir.path().display();
    let yaml = format!(
        "duckdb: {{ extension_dir: {ext} }}\nstorage: {{ url: {d}/o }}\ncatalog: {{ url: {d}/c.ducklake }}\ningest: {{ wal_dir: {d}/w, max_sample_age: 30d, durability: memory }}\nfleet: {{ dir: {d}/fleet }}\nmaterialize: {{ path: \"\" }}\n"
    );
    let node = Node::open(Config::from_layers(Some(&yaml), &[]).unwrap()).await.unwrap();
    let now = now_ms() / 60_000 * 60_000;
    let steps = 6 * 240;
    for chunk in 0..(steps / 60) {
        let mut rows = Rows::default();
        for s in 0..2000u64 {
            let labels: Labels = [("job".to_string(), format!("api-{}", s % 10)), ("instance".to_string(), format!("i{s}"))].into();
            let id = engine::ids::series_id("default", "local", "http_requests_total", &labels);
            rows.series.push(MetricsSeriesRow { series_id: id, metric_name: "http_requests_total".into(), labels, ..Default::default() });
            for k in chunk * 60..(chunk + 1) * 60 {
                rows.points.push(MetricsPointsRow {
                    series_id: id,
                    timestamp_ms: now - (steps - k) * 15_000,
                    value: (k * (1 + s as i64 % 13)) as f64,
                    ..Default::default()
                });
            }
        }
        node.ingest(rows).await.unwrap();
    }
    node.flush_and_checkpoint().await.unwrap();
    let doc = format!(
        r#"{{"tqs":"3","id":"q","kind":"timeseries","time":{{"from":{},"to":{now},"step":"60s"}},"queries":[{{"name":"r","metric":"http_requests_total","fn":{{"op":"rate","window":"5m"}},"agg":{{"op":"sum","by":["job"]}}}}]}}"#,
        now - 6 * 3_600_000 + 600_000
    );
    let t = tqs::Tqs::parse(&doc).unwrap();
    let r = t.validate(now).unwrap();
    let (sql, params) = tqs::compile::timeseries_sql(&t, &r).unwrap();
    let plan = node
        .with_conn(move |c| {
            let started = std::time::Instant::now();
            let mut stmt = c.prepare(&format!("EXPLAIN ANALYZE {sql}"))?;
            let mut rows = stmt.query(duckdb::params_from_iter(params.iter()))?;
            let mut out = String::new();
            while let Some(row) = rows.next()? {
                out.push_str(&row.get::<_, String>(1)?);
            }
            Ok(format!("{out}\ntotal {:?}", started.elapsed()))
        })
        .await
        .unwrap();
    println!("{plan}");
}
