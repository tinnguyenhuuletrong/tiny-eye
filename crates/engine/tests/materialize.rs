//! Materialized panels answer exactly what the raw query answers on the same grid: stored points
//! for the covered range, computed steps before it and in the unsettled tail.

#![allow(clippy::unwrap_used)]

use engine::node::now_ms;
use engine::tables::*;
use engine::{Config, Node, Rows};
use std::collections::BTreeMap;
use std::sync::Arc;

fn config(dir: &std::path::Path) -> Config {
    let ext = concat!(env!("CARGO_MANIFEST_DIR"), "/../../.tools/duckdb_ext");
    let yaml = format!(
        "duckdb: {{ extension_dir: {ext}, threads: 2, memory_limit: 1GB }}\nstorage: {{ url: {d}/objects }}\ncatalog: {{ url: {d}/catalog.ducklake }}\n\
         ingest: {{ wal_dir: {d}/wal, max_sample_age: 1d }}\nmaterialize: {{ path: {d}/mat.duckdb, resolution: 15s, settle: 5m }}\n\
         coordinator: {{ hot_grace: 1ms }}\n",
        d = dir.display()
    );
    Config::from_layers(Some(&yaml), &[]).unwrap()
}

/// Deterministic noise in [0, 1).
fn noise(i: i64, s: u64) -> f64 {
    let x = (i as u64).wrapping_mul(6364136223846793005).wrapping_add(s.wrapping_mul(1442695040888963407));
    ((x >> 11) as f64) / ((1u64 << 53) as f64)
}

/// 3 h of samples every 15 s ending at `end`: a counter per (job, code) with one reset, and a
/// gauge per host.
fn rows(end: i64, from: i64) -> Rows {
    let mut r = Rows::default();
    let mut add = |name: &str, labels: Labels, f: &dyn Fn(i64) -> f64| {
        let id = engine::ids::series_id("default", "local", name, &labels);
        r.series.push(MetricsSeriesRow { series_id: id, metric_name: name.into(), labels, ..Default::default() });
        let mut t = from;
        let mut i = 0;
        while t <= end {
            r.points.push(MetricsPointsRow { series_id: id, timestamp_ms: t, value: f(i), ..Default::default() });
            t += 15_000;
            i += 1;
        }
    };
    for (j, job) in ["api", "web", "worker"].iter().enumerate() {
        for (c, code) in ["200", "500"].iter().enumerate() {
            let s = (j * 2 + c) as u64;
            let labels: Labels = [("job".to_string(), job.to_string()), ("code".to_string(), code.to_string())].into();
            // A restart of api/500 halfway through resets its counter.
            add("reqs_total", labels, &move |i| {
                let base: f64 = (0..i).map(|k| 1.0 + 9.0 * noise(k, s)).sum();
                if s == 1 && i > 360 { base - (0..360).map(|k| 1.0 + 9.0 * noise(k, s)).sum::<f64>() } else { base }
            });
        }
    }
    for h in 0..3u64 {
        add("temp", [("host".to_string(), format!("h{h}"))].into(), &move |i| 40.0 + 20.0 * noise(i, 100 + h));
    }
    r
}

fn doc(id: &str, body: &str) -> serde_json::Value {
    let mut v: serde_json::Value = serde_json::from_str(body).unwrap();
    v["tqs"] = "3".into();
    v["id"] = id.into();
    v["kind"] = "timeseries".into();
    v
}

fn panels() -> Vec<(String, serde_json::Value)> {
    vec![
        (
            "t/rate".into(),
            doc(
                "rate",
                r#"{"queries":[{"name":"r","metric":"reqs_total","fn":{"op":"rate","window":"5m"},"agg":{"op":"sum","by":["job"]}}]}"#,
            ),
        ),
        (
            "t/temp".into(),
            doc(
                "temp",
                r#"{"queries":[{"name":"m","metric":"temp","fn":{"op":"max_over_time","window":"2m"},"agg":{"op":"max","by":["host"]}}]}"#,
            ),
        ),
        (
            "t/errors".into(),
            doc(
                "errors",
                r#"{"queries":[
                    {"name":"e","metric":"reqs_total","match":[{"label":"code","op":"=","value":"500"}],"fn":{"op":"increase","window":"10m"},"agg":{"op":"sum","by":["job"]},"hidden":true},
                    {"name":"a","metric":"reqs_total","fn":{"op":"increase","window":"10m"},"agg":{"op":"sum","by":["job"]},"hidden":true}],
                   "formulas":[{"name":"pct","expr":"100 * e / a"}]}"#,
            ),
        ),
        (
            "t/top".into(),
            doc(
                "top",
                r#"{"queries":[{"name":"r","metric":"reqs_total","fn":{"op":"irate","window":"1m"},"agg":{"op":"sum","by":["job","code"]}}],"top":{"n":2}}"#,
            ),
        ),
        (
            "t/having".into(),
            doc(
                "having",
                r#"{"queries":[{"name":"g","metric":"temp","fn":{"op":"last"},"agg":{"op":"avg","by":["host"]},"having":{"op":">","value":50}}]}"#,
            ),
        ),
    ]
}

type Out = BTreeMap<(String, String), Vec<(i64, f64)>>;

fn shape(o: &engine::query::QueryOutput) -> Out {
    o.series.iter().map(|s| ((s.output.clone(), s.key.clone()), s.points.clone())).collect()
}

fn assert_same(name: &str, mat: &Out, raw: &Out) {
    assert_eq!(mat.keys().collect::<Vec<_>>(), raw.keys().collect::<Vec<_>>(), "{name}: same series");
    for (k, a) in mat {
        let b = &raw[k];
        assert_eq!(a.iter().map(|p| p.0).collect::<Vec<_>>(), b.iter().map(|p| p.0).collect::<Vec<_>>(), "{name} {k:?}: same timestamps");
        for ((t, x), (_, y)) in a.iter().zip(b) {
            let tol = 1e-9 * x.abs().max(y.abs()).max(1.0);
            assert!((x - y).abs() <= tol, "{name} {k:?} at {t}: materialized {x} vs raw {y}");
        }
    }
}

async fn node(dir: &std::path::Path) -> Arc<Node> {
    Node::open(config(dir)).await.unwrap()
}

#[tokio::test]
async fn materialized_reads_equal_raw_reads() {
    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path()).await;
    let now = now_ms() / 15_000 * 15_000;
    n.ingest(rows(now - 60_000, now - 3 * 3_600_000)).await.unwrap();
    n.flush_and_checkpoint().await.unwrap();
    // The newest minute stays in the hot tail.
    n.ingest(rows(now, now - 45_000)).await.unwrap();

    n.set_materialized(panels());
    let stored = n.materialize_since(now - 2 * 3_600_000, now).await.unwrap();
    assert!(stored > 0, "points were stored");

    // (range, step): inside the coverage, reaching before it (computed head), and steps that are
    // multiples of the 15 s resolution. Every range ends in the unsettled tail (computed).
    for (range, step) in [(3_600_000, 15_000), (3 * 3_600_000, 60_000), (150 * 60_000, 120_000), (20 * 60_000, 30_000)] {
        let (from, to) = (now - range, now);
        for (name, mut q) in panels() {
            q["time"] = serde_json::json!({"from": from + 7_000, "to": to, "step": format!("{}ms", step)});
            let mat = n.query(&q.to_string()).await.unwrap();
            let info = mat.materialized.clone().unwrap_or_else(|| panic!("{name}: answered from the store"));
            assert!(info.computed_steps > 0, "{name}: the unsettled tail is computed");
            if range <= 2 * 3_600_000 - 600_000 {
                assert!(info.stored_points > 0, "{name}: stored points used for {range} ms");
            }
            // The raw query on the aligned grid the materialized answer uses.
            let (f, t) = ((from + 7_000 + step - 1) / step * step, to / step * step);
            q["time"] = serde_json::json!({"from": f, "to": t, "step": format!("{}ms", step)});
            let raw = n.query_with(&q.to_string(), false).await.unwrap();
            assert!(raw.materialized.is_none());
            assert!(!raw.series.is_empty(), "{name}: the comparison is not vacuous");
            assert_same(&format!("{name} {range}ms/{step}ms"), &shape(&mat), &shape(&raw));
        }
    }
}

#[tokio::test]
async fn reads_fall_back_to_raw_when_the_store_cannot_help() {
    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path()).await;
    let now = now_ms() / 15_000 * 15_000;
    n.ingest(rows(now, now - 3_600_000)).await.unwrap();
    n.set_materialized(panels());
    n.materialize_since(now - 3_600_000, now).await.unwrap();
    let (_, mut q) = panels().remove(0);

    // A step that is not a multiple of the resolution.
    q["time"] = serde_json::json!({"from": "-1h", "step": "10s"});
    assert!(n.query(&q.to_string()).await.unwrap().materialized.is_none());

    // A different query (window changed) is not the registered panel.
    q["time"] = serde_json::json!({"from": "-1h", "step": "15s"});
    q["queries"][0]["fn"]["window"] = "1m".into();
    assert!(n.query(&q.to_string()).await.unwrap().materialized.is_none());

    // Unregistered: points are forgotten, reads are raw.
    let (_, mut q) = panels().remove(0);
    q["time"] = serde_json::json!({"from": "-1h", "step": "15s"});
    n.set_materialized(vec![]);
    n.materialize(now).await.unwrap();
    assert!(n.query(&q.to_string()).await.unwrap().materialized.is_none());

    // Registered again from scratch with the background call: no backfill, only the horizon step.
    n.set_materialized(panels());
    n.materialize(now).await.unwrap();
    let info = n.query(&q.to_string()).await.unwrap().materialized.unwrap();
    assert!(info.stored_points <= 3, "only the settled step is stored, got {}", info.stored_points);
    assert!(info.computed_steps > 200, "the rest of the hour is computed");
}

#[tokio::test]
async fn materialized_points_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let now = now_ms() / 15_000 * 15_000;
    {
        let n = node(dir.path()).await;
        n.ingest(rows(now, now - 3_600_000)).await.unwrap();
        n.flush_and_checkpoint().await.unwrap();
        n.set_materialized(panels());
        n.materialize_since(now - 3_600_000, now).await.unwrap();
    }
    let n = node(dir.path()).await;
    n.set_materialized(panels());
    let (_, mut q) = panels().remove(0);
    q["time"] = serde_json::json!({"from": "-1h", "step": "15s"});
    let info = n.query(&q.to_string()).await.unwrap().materialized.unwrap();
    assert!(info.stored_points > 100, "points stored before the restart are used: {}", info.stored_points);
}
