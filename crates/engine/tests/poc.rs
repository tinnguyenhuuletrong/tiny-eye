//! Proof of concept on this machine: every storage assumption the engine builds on, as a
//! test that keeps running. Local-disk DuckLake (DuckDB-file catalog), so no services needed.
//!
//! P1 generated schema → Rust Parquet writer → `ducklake_add_data_files` (MAP, BLOB, TIMESTAMP_NS)
//! P2 hot tail: in-memory table + watermark view; counted once before flush, after commit, after drop
//! P3 compaction swap: `DELETE … WHERE filename IN` + add L1 in one transaction, rows unchanged
//! P4 retention: expired files are deleted by DuckLake
//! P5 TQS runs unchanged on the lake + hot-tail views
//! P6 bloom filter present on trace_id in every row group

#![allow(clippy::unwrap_used)]

use duckdb::Connection;
use engine::tables::*;
use engine::{duck, parquet};

fn ext_dir() -> String {
    std::env::var("TINY_EYE_DUCKDB_EXT").unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../../.tools/duckdb_ext").to_string())
}

fn lake(dir: &std::path::Path) -> Connection {
    let conn = duck::open(&duck::DuckSettings { extension_dir: ext_dir(), threads: 2, memory_limit: "1GB".into() }).unwrap();
    let data = dir.join("objects");
    std::fs::create_dir_all(&data).unwrap();
    duck::attach_lake(&conn, dir.join("catalog.ducklake").to_str().unwrap(), &format!("{}/", data.display())).unwrap();
    for t in Table::ALL {
        conn.execute_batch(&t.ddl(&format!("lake.{}", t.name()))).unwrap();
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS lake.applied_envelopes (envelope_id VARCHAR, region VARCHAR, node_id VARCHAR, node_epoch BIGINT, seq_to BIGINT, files INTEGER)",
    )
    .unwrap();
    conn
}

fn points(series: u64, from_ms: i64, n: i64) -> arrow::array::RecordBatch {
    let mut b = MetricsPointsBuilder::default();
    for s in 0..series {
        for i in 0..n {
            b.append(&MetricsPointsRow {
                tenant: "default".into(),
                region: "local".into(),
                series_id: s,
                timestamp_ms: from_ms + i * 15_000,
                value: (i * 10) as f64,
                is_stale: false,
                positive_delta: None,
            });
        }
    }
    b.finish()
}

fn put(dir: &std::path::Path, table: Table, name: &str, batch: &arrow::array::RecordBatch) -> String {
    let sorted = parquet::sort(table, batch).unwrap();
    let bytes = parquet::write(table, &[sorted], 8192).unwrap();
    let path = dir.join("objects").join(format!("{}-{name}.parquet", table.name()));
    std::fs::write(&path, &bytes).unwrap();
    path.display().to_string()
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

#[test]
fn p1_writer_files_register_in_ducklake() {
    let dir = tempfile::tempdir().unwrap();
    let conn = lake(dir.path());
    println!("duckdb {}", duck::version(&conn).unwrap());
    // Spans exercise MAP, BLOB (FixedSizeBinary 16), TIMESTAMP_NS, nullable UBIGINT.
    let mut sb = SpansBuilder::default();
    for i in 0..100u64 {
        let mut attrs = Labels::new();
        attrs.insert("http.route".into(), format!("/r/{}", i % 3));
        sb.append(&SpansRow {
            tenant: "default".into(),
            region: "local".into(),
            trace_id: [(i % 7) as u8; 16],
            span_id: i + 1,
            parent_span_id: if i % 7 == i { None } else { Some((i % 7) + 1) },
            start_time_ns: 1_762_740_000_000_000_000 + i as i64 * 1_000_000,
            duration_ns: 1_000 * i,
            service_name: "checkout".into(),
            span_name: "op".into(),
            span_kind: 2,
            status_code: if i % 10 == 0 { 2 } else { 0 },
            status_message: None,
            resource_attrs: Labels::new(),
            span_attrs: attrs,
        });
    }
    let spans = put(dir.path(), Table::Spans, "a", &sb.finish());
    let pts = put(dir.path(), Table::MetricsPoints, "a", &points(3, 1_762_740_000_000, 10));
    conn.execute_batch(&format!(
        "BEGIN; CALL ducklake_add_data_files('lake', 'spans', '{spans}'); CALL ducklake_add_data_files('lake', 'metrics_points', '{pts}'); COMMIT;"
    ))
    .unwrap();
    assert_eq!(count(&conn, "SELECT count(*) FROM lake.spans"), 100);
    assert_eq!(count(&conn, "SELECT count(*) FROM lake.spans WHERE span_attrs['http.route'] = '/r/1'"), 33);
    assert_eq!(count(&conn, "SELECT count(*) FROM lake.spans WHERE lower(hex(trace_id)) = repeat('03', 16)"), 14);
    assert_eq!(count(&conn, "SELECT count(*) FROM lake.metrics_points"), 30);
    // A schema mismatch is refused at commit time (E3): a file without the `labels` MAP.
    let bad = put(dir.path(), Table::MetricsPoints, "bad", &points(1, 0, 1));
    assert!(conn.execute_batch(&format!("CALL ducklake_add_data_files('lake', 'metrics_series', '{bad}')")).is_err());
}

#[test]
fn p2_hot_tail_counted_once() {
    let dir = tempfile::tempdir().unwrap();
    let conn = lake(dir.path());
    // Hot rows live in the in-memory database with the flush sequence they belong to.
    conn.execute_batch(&format!(
        "CREATE TABLE memory.main.hot_metrics_points ({}, _seq BIGINT);
         CREATE VIEW memory.main.metrics_points AS
           SELECT * FROM lake.metrics_points
           UNION ALL BY NAME
           SELECT * EXCLUDE (_seq) FROM memory.main.hot_metrics_points
           WHERE _seq > (SELECT coalesce(max(seq_to), 0) FROM lake.applied_envelopes WHERE node_id = 'n0' AND node_epoch = 1);",
        Table::MetricsPoints.ddl_columns()
    ))
    .unwrap();
    let batch = points(2, 1_762_740_000_000, 1000); // 2,000 rows
    {
        let mut app = conn.appender_to_catalog_and_db("hot_metrics_points", "memory", "main").unwrap();
        // Arrow append needs the `_seq` column too.
        let mut cols = batch.columns().to_vec();
        cols.push(std::sync::Arc::new(arrow::array::Int64Array::from(vec![1i64; batch.num_rows()])));
        let mut fields: Vec<arrow::datatypes::FieldRef> = batch.schema().fields().iter().cloned().collect();
        fields.push(std::sync::Arc::new(arrow::datatypes::Field::new("_seq", arrow::datatypes::DataType::Int64, false)));
        let with_seq = arrow::array::RecordBatch::try_new(std::sync::Arc::new(arrow::datatypes::Schema::new(fields)), cols).unwrap();
        app.append_record_batch(with_seq).unwrap();
    }
    let n = || count(&conn, "SELECT count(*) FROM memory.main.metrics_points");
    assert_eq!(n(), 2000, "before flush: hot only");
    // Flush seq 1 and commit its envelope in one catalog transaction.
    let f = put(dir.path(), Table::MetricsPoints, "seq1", &batch);
    conn.execute_batch(&format!(
        "BEGIN; CALL ducklake_add_data_files('lake', 'metrics_points', '{f}');
         INSERT INTO lake.applied_envelopes VALUES ('e1', 'local', 'n0', 1, 1, 1); COMMIT;"
    ))
    .unwrap();
    assert_eq!(n(), 2000, "after commit, still in RAM: counted once (naive union would give 4000)");
    conn.execute_batch("DELETE FROM memory.main.hot_metrics_points WHERE _seq <= 1").unwrap();
    assert_eq!(n(), 2000, "after drop: lake only");
    // One transaction can read both databases (the query pins both snapshots together).
    conn.execute_batch("BEGIN TRANSACTION READ ONLY").unwrap_or_else(|_| conn.execute_batch("BEGIN").unwrap());
    assert_eq!(n(), 2000);
    conn.execute_batch("COMMIT").unwrap();
}

#[test]
fn p3_compaction_swap_and_p4_retention() {
    let dir = tempfile::tempdir().unwrap();
    let conn = lake(dir.path());
    let mut l0 = vec![];
    for i in 0..5 {
        let f = put(dir.path(), Table::MetricsPoints, &format!("l0-{i}"), &points(4, 1_762_740_000_000 + i * 600_000, 40));
        conn.execute_batch(&format!("CALL ducklake_add_data_files('lake', 'metrics_points', '{f}')")).unwrap();
        l0.push(f);
    }
    let before = count(&conn, "SELECT count(*) FROM lake.metrics_points");
    let sum_before: f64 = conn.query_row("SELECT sum(value) FROM lake.metrics_points", [], |r| r.get(0)).unwrap();
    // Compactor: read L0 sorted, compute positive_delta, write L1 with our writer, swap atomically.
    let list = l0.iter().map(|f| format!("'{f}'")).collect::<Vec<_>>().join(", ");
    let mut stmt = conn
        .prepare(&format!(
            "SELECT tenant, region, series_id, timestamp_ms, value, is_stale,
                    CASE WHEN lag(value) OVER w IS NULL THEN NULL WHEN value >= lag(value) OVER w THEN value - lag(value) OVER w ELSE value END AS positive_delta
             FROM read_parquet([{list}]) WINDOW w AS (PARTITION BY series_id ORDER BY timestamp_ms) ORDER BY series_id, timestamp_ms"
        ))
        .unwrap();
    let batches: Vec<_> = stmt.query_arrow([]).unwrap().collect();
    let bytes = parquet::write(Table::MetricsPoints, &batches, 122_880).unwrap();
    let l1 = dir.path().join("objects/metrics_points-l1.parquet").display().to_string();
    std::fs::write(&l1, bytes).unwrap();
    conn.execute_batch(&format!(
        "BEGIN; DELETE FROM lake.metrics_points WHERE filename IN ({list}); CALL ducklake_add_data_files('lake', 'metrics_points', '{l1}'); COMMIT;"
    ))
    .unwrap();
    assert_eq!(count(&conn, "SELECT count(*) FROM lake.metrics_points"), before);
    let sum_after: f64 = conn.query_row("SELECT sum(value) FROM lake.metrics_points", [], |r| r.get(0)).unwrap();
    assert_eq!(sum_before, sum_after);
    assert_eq!(count(&conn, "SELECT count(DISTINCT filename) FROM lake.metrics_points"), 1);
    assert!(count(&conn, "SELECT count(*) FROM lake.metrics_points WHERE positive_delta IS NOT NULL") > 0);
    // Retention: expire old snapshots, then DuckLake deletes the swapped-out L0 files it did not write (E6).
    conn.execute_batch("CALL ducklake_expire_snapshots('lake', older_than => to_timestamp(epoch(now()) + 1)); CALL ducklake_cleanup_old_files('lake', cleanup_all => true);").unwrap();
    let left = l0.iter().filter(|f| std::path::Path::new(f).exists()).count();
    assert_eq!(left, 0, "expired L0 files are deleted by DuckLake");
    assert!(std::path::Path::new(&l1).exists());
}

#[test]
fn p5_tqs_on_lake_views() {
    let dir = tempfile::tempdir().unwrap();
    let conn = lake(dir.path());
    let t0 = 1_762_740_000_000i64;
    let mut sb = MetricsSeriesBuilder::default();
    for s in 0..2u64 {
        let mut l = Labels::new();
        l.insert("job".into(), format!("j{s}"));
        sb.append(&MetricsSeriesRow {
            tenant: "default".into(),
            region: "local".into(),
            day_ms: 0,
            series_id: s,
            metric_name: "reqs_total".into(),
            metric_type: 1,
            unit: None,
            labels: l,
        });
    }
    let f1 = put(dir.path(), Table::MetricsSeries, "s", &sb.finish());
    let f2 = put(dir.path(), Table::MetricsPoints, "p", &points(2, t0, 100));
    conn.execute_batch(&format!(
        "CALL ducklake_add_data_files('lake', 'metrics_series', '{f1}'); CALL ducklake_add_data_files('lake', 'metrics_points', '{f2}');
         CREATE VIEW memory.main.metrics_series AS SELECT series_id, any_value(metric_name) AS metric_name, any_value(labels) AS labels FROM lake.metrics_series GROUP BY series_id;
         CREATE VIEW memory.main.metrics_points AS SELECT * FROM lake.metrics_points;"
    ))
    .unwrap();
    let doc = serde_json::json!({"tqs": "3", "id": "p5", "kind": "timeseries",
        "time": {"from": t0 + 600_000, "to": t0 + 1_200_000, "step": "1m"},
        "queries": [{"name": "r", "metric": "reqs_total", "fn": {"op": "rate", "window": "5m"}, "agg": {"op": "sum", "by": ["job"]}}]});
    let q = tqs::Tqs::parse(&doc.to_string()).unwrap();
    let r = q.validate(t0).unwrap();
    let rows = tqs::compile::run(&conn, &q, &r).unwrap();
    // 10 per 15 s scrape = 0.6667/s, 11 steps × 2 jobs.
    assert_eq!(rows.len(), 22);
    for row in rows {
        assert!((row.value - 10.0 / 15.0).abs() < 1e-9, "{row:?}");
    }
}

#[test]
fn p6_bloom_on_trace_id() {
    use ::parquet::file::reader::{FileReader, SerializedFileReader};
    let mut sb = SpansBuilder::default();
    for i in 0..20_000u64 {
        let mut tid = [0u8; 16];
        tid[..8].copy_from_slice(&(i / 4).to_le_bytes());
        sb.append(&SpansRow { trace_id: tid, span_id: i, service_name: "s".into(), span_name: "n".into(), ..Default::default() });
    }
    let batch = parquet::sort(Table::Spans, &sb.finish()).unwrap();
    let bytes = parquet::write(Table::Spans, &[batch], 8192).unwrap();
    let r = SerializedFileReader::new(bytes).unwrap();
    let md = r.metadata();
    assert_eq!(md.num_row_groups(), 3);
    let idx = Table::Spans.column_names().iter().position(|c| *c == "trace_id").unwrap();
    for rg in md.row_groups() {
        assert!(rg.column(idx).bloom_filter_offset().is_some(), "bloom filter on every row group");
    }
}

/// P7 The ingest arena is an in-memory DuckDB table: the Arrow appender must take every
/// generated column type (MAP, BLOB trace ids, TIMESTAMP_NS, nullable ints), and DuckDB's
/// Arrow export must give back batches the Parquet writer accepts.
#[test]
fn p7_arrow_appender_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let conn = lake(dir.path());
    conn.execute_batch(&Table::Spans.ddl("memory.main.hot_spans")).unwrap();
    let mut sb = SpansBuilder::default();
    for i in 0..1000u64 {
        let mut attrs = Labels::new();
        attrs.insert("k".into(), format!("v{}", i % 5));
        sb.append(&SpansRow {
            tenant: "t".into(),
            region: "r".into(),
            trace_id: [(i % 251) as u8; 16],
            span_id: i,
            parent_span_id: (i % 2 == 0).then_some(i + 1),
            start_time_ns: 1_762_740_000_000_000_000 + i as i64,
            service_name: "svc".into(),
            span_name: "op".into(),
            span_attrs: attrs,
            ..Default::default()
        });
    }
    conn.appender_to_catalog_and_db("hot_spans", "memory", "main").unwrap().append_record_batch(sb.finish()).unwrap();
    assert_eq!(count(&conn, "SELECT count(*) FROM memory.main.hot_spans WHERE span_attrs['k'] = 'v3'"), 200);
    assert_eq!(count(&conn, "SELECT count(*) FROM memory.main.hot_spans WHERE parent_span_id IS NULL"), 500);
    assert_eq!(count(&conn, "SELECT count(*) FROM memory.main.hot_spans WHERE octet_length(trace_id) = 16"), 1000);
    // Export sorted by DuckDB, written by our writer, registered in DuckLake.
    let batches: Vec<_> =
        conn.prepare("SELECT * FROM memory.main.hot_spans ORDER BY trace_id, start_time_ns").unwrap().query_arrow([]).unwrap().collect();
    let bytes = parquet::write(Table::Spans, &batches, 8192).unwrap();
    let f = dir.path().join("objects/spans-export.parquet").display().to_string();
    std::fs::write(&f, bytes).unwrap();
    conn.execute_batch(&format!("CALL ducklake_add_data_files('lake', 'spans', '{f}')")).unwrap();
    assert_eq!(count(&conn, "SELECT count(*) FROM lake.spans WHERE span_attrs['k'] = 'v3'"), 200);
}
