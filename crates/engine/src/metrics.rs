//! Self-metrics (SPEC §13), exposed in Prometheus text format at GET /metrics.

use crate::tables::Table;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default)]
pub struct Metrics {
    pub ingested_rows: [AtomicU64; Table::ALL.len()],
    pub rejected_out_of_bounds: AtomicU64,
    pub rejected_overload: AtomicU64,
    pub flushes: AtomicU64,
    pub flush_ms: AtomicU64,
    pub flushed_bytes: AtomicU64,
    pub checkpoints: AtomicU64,
    pub compactions: AtomicU64,
    pub queries: AtomicU64,
    pub query_errors: AtomicU64,
    pub query_ms: AtomicU64,
    pub materialized_points: AtomicU64,
    pub materialized_reads: AtomicU64,
    pub materialized_trimmed: AtomicU64,
    /// Gauges, set by the background task each tick.
    pub materialized_stored: AtomicU64,
    pub materialized_panels: AtomicU64,
    pub materialized_tick_ms: AtomicU64,
    pub fleet_publishes: AtomicU64,
    pub fleet_errors: AtomicU64,
    /// Other nodes whose catalog copies queries read, and the oldest copy's publish time (0 = none).
    pub fleet_nodes: AtomicU64,
    pub fleet_oldest_published_ms: AtomicU64,
}

impl Metrics {
    pub fn ingested(&self, t: Table, n: u64) {
        if let Some(i) = Table::ALL.iter().position(|x| *x == t) {
            self.ingested_rows[i].fetch_add(n, Ordering::Relaxed);
        }
    }

    pub fn render(&self, hot_bytes: u64) -> String {
        let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
        let mut s = String::new();
        let mut counter = |name: &str, help: &str, rows: Vec<(String, u64)>| {
            let _ = writeln!(s, "# HELP tiny_eye_{name} {help}\n# TYPE tiny_eye_{name} counter");
            for (labels, v) in rows {
                let _ = writeln!(s, "tiny_eye_{name}{labels} {v}");
            }
        };
        counter(
            "ingested_rows_total",
            "Rows admitted, by table.",
            Table::ALL.iter().enumerate().map(|(i, t)| (format!("{{table=\"{}\"}}", t.name()), g(&self.ingested_rows[i]))).collect(),
        );
        counter(
            "rejected_rows_total",
            "Rows refused, by reason.",
            vec![
                ("{reason=\"out_of_bounds\"}".into(), g(&self.rejected_out_of_bounds)),
                ("{reason=\"overload\"}".into(), g(&self.rejected_overload)),
            ],
        );
        counter("flushes_total", "Flushes written to object storage.", vec![(String::new(), g(&self.flushes))]);
        counter("flush_milliseconds_total", "Time spent flushing.", vec![(String::new(), g(&self.flush_ms))]);
        counter("flushed_bytes_total", "Parquet bytes written by flushes.", vec![(String::new(), g(&self.flushed_bytes))]);
        counter("checkpoints_total", "Catalog checkpoints that applied envelopes.", vec![(String::new(), g(&self.checkpoints))]);
        counter("compactions_total", "L0 to L1 compactions.", vec![(String::new(), g(&self.compactions))]);
        counter("queries_total", "TQS queries run.", vec![(String::new(), g(&self.queries))]);
        counter("query_errors_total", "TQS queries that failed.", vec![(String::new(), g(&self.query_errors))]);
        counter("query_milliseconds_total", "Time spent running queries.", vec![(String::new(), g(&self.query_ms))]);
        counter("materialized_points_total", "Points stored for materialized panels.", vec![(String::new(), g(&self.materialized_points))]);
        counter(
            "materialized_reads_total",
            "Queries answered from materialized panel points.",
            vec![(String::new(), g(&self.materialized_reads))],
        );
        counter(
            "materialized_trimmed_points_total",
            "Stored points dropped because a panel passed materialize.max_points_per_panel.",
            vec![(String::new(), g(&self.materialized_trimmed))],
        );
        counter(
            "fleet_publishes_total",
            "Catalog copies published for the other nodes of the fleet.",
            vec![(String::new(), g(&self.fleet_publishes))],
        );
        counter(
            "fleet_errors_total",
            "Failed fleet publishes, refreshes and catalog copy attaches.",
            vec![(String::new(), g(&self.fleet_errors))],
        );
        let oldest = g(&self.fleet_oldest_published_ms);
        let age = if oldest == 0 { 0.0 } else { (crate::node::now_ms() - oldest as i64).max(0) as f64 / 1000.0 };
        let _ = writeln!(
            s,
            "# HELP tiny_eye_fleet_nodes Other nodes sharing storage whose data queries read.\n# TYPE tiny_eye_fleet_nodes gauge\ntiny_eye_fleet_nodes {}\n\
             # HELP tiny_eye_fleet_oldest_copy_age_seconds Age of the oldest other-node catalog copy queries read (0 when alone).\n\
             # TYPE tiny_eye_fleet_oldest_copy_age_seconds gauge\ntiny_eye_fleet_oldest_copy_age_seconds {age}",
            g(&self.fleet_nodes)
        );
        for (name, help, v) in [
            ("materialized_stored_points", "Points now stored for materialized panels.", &self.materialized_stored),
            ("materialized_panels", "Panels currently materialized.", &self.materialized_panels),
            ("materialized_tick_milliseconds", "Duration of the last materialization tick.", &self.materialized_tick_ms),
        ] {
            let _ = writeln!(s, "# HELP tiny_eye_{name} {help}\n# TYPE tiny_eye_{name} gauge\ntiny_eye_{name} {}", g(v));
        }
        let _ = writeln!(
            s,
            "# HELP tiny_eye_hot_bytes Bytes buffered in the ingest arena / hot tail.\n# TYPE tiny_eye_hot_bytes gauge\ntiny_eye_hot_bytes {hot_bytes}"
        );
        s
    }
}
