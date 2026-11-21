//! TQS execution on the node (SPEC §7): validate → compile → run on a pooled connection in one
//! read transaction, so the lake snapshot and the hot tail are read consistently (E7).

use crate::node::{Node, now_ms};
use anyhow::Result;
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::time::Instant;

#[derive(Debug, Clone, Serialize)]
pub struct SeriesOut {
    pub output: String,
    pub key: String,
    pub labels: BTreeMap<String, String>,
    pub points: Vec<(i64, f64)>,
}

#[derive(Debug, Clone, Serialize)]
pub struct QueryOutput {
    pub id: String,
    pub snapshot_id: i64,
    pub freshness_s: f64,
    pub series: Vec<SeriesOut>,
    pub rows: Vec<BTreeMap<String, String>>,
    pub rows_total: i64,
    pub truncated: bool,
    pub elapsed_ms: f64,
}

#[derive(Debug)]
pub enum QueryError {
    /// Every problem in the document (SPEC §8.12).
    Invalid(Vec<tqs::Problem>),
    Failed(anyhow::Error),
}

#[derive(Debug, Clone, Serialize)]
pub struct CatalogMetric {
    pub name: String,
    /// counter | gauge | histogram | summary | unknown
    pub r#type: String,
    pub series: i64,
    pub label_keys: Vec<String>,
    /// Up to `VALUES_PER_LABEL` distinct values per key.
    pub label_values: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CatalogService {
    pub name: String,
    pub span_names: Vec<String>,
    pub log_lines: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Catalog {
    pub metrics: Vec<CatalogMetric>,
    pub services: Vec<CatalogService>,
    pub truncated: bool,
}

/// `k=v,k=v` → map. Values containing `,` are split ambiguously; the key string stays exact.
pub fn labels_of_key(key: &str) -> BTreeMap<String, String> {
    key.split(',').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

const CATALOG_LIMIT: usize = 500;
const VALUES_PER_LABEL: usize = 50;

fn text_list(v: duckdb::types::Value) -> Vec<String> {
    match v {
        duckdb::types::Value::List(v) => {
            v.into_iter().filter_map(|x| if let duckdb::types::Value::Text(t) = x { Some(t) } else { None }).collect()
        }
        _ => vec![],
    }
}

impl Node {
    pub fn parse_and_validate(json: &str, now: i64) -> Result<(tqs::Tqs, tqs::Resolved), Vec<tqs::Problem>> {
        let doc = tqs::Tqs::parse(json).map_err(|p| vec![p])?;
        let r = doc.validate(now)?;
        Ok((doc, r))
    }

    pub async fn query(&self, json: &str) -> Result<QueryOutput, QueryError> {
        let started = Instant::now();
        let (doc, r) = Self::parse_and_validate(json, now_ms()).map_err(QueryError::Invalid)?;
        self.metrics.queries.fetch_add(1, Ordering::Relaxed);
        let res = self
            .with_conn(move |conn| {
                conn.execute_batch("BEGIN TRANSACTION")?;
                let out = (|| -> Result<QueryOutput> {
                    let snapshot_id: i64 = conn.query_row("SELECT max(snapshot_id) FROM ducklake_snapshots('lake')", [], |r| r.get(0))?;
                    let mut out = QueryOutput {
                        id: doc.id.clone(),
                        snapshot_id,
                        // The hot tail is part of every read: rows are visible once acknowledged.
                        freshness_s: 0.0,
                        series: vec![],
                        rows: vec![],
                        rows_total: 0,
                        truncated: false,
                        elapsed_ms: 0.0,
                    };
                    match doc.kind {
                        tqs::Kind::Timeseries | tqs::Kind::Table => {
                            let rows = tqs::compile::run(conn, &doc, &r)?;
                            out.rows_total = rows.len() as i64;
                            for row in rows {
                                match out.series.last_mut() {
                                    Some(s) if s.output == row.output && s.key == row.key => s.points.push((row.t, row.value)),
                                    _ => out.series.push(SeriesOut {
                                        labels: labels_of_key(&row.key),
                                        output: row.output,
                                        key: row.key,
                                        points: vec![(row.t, row.value)],
                                    }),
                                }
                            }
                        }
                        _ => {
                            let rows = tqs::compile::run_list(conn, &doc, &r)?;
                            out.rows_total = rows.len() as i64;
                            out.truncated = rows.len() as u32 >= doc.limit.unwrap_or(100) && doc.kind != tqs::Kind::Trace;
                            out.rows = rows.into_iter().map(|cells| cells.into_iter().collect()).collect();
                        }
                    }
                    Ok(out)
                })();
                let _ = conn.execute_batch("COMMIT");
                out
            })
            .await;
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        self.metrics.query_ms.fetch_add(ms as u64, Ordering::Relaxed);
        match res {
            Ok(mut o) => {
                o.elapsed_ms = ms;
                Ok(o)
            }
            Err(e) => {
                self.metrics.query_errors.fetch_add(1, Ordering::Relaxed);
                Err(QueryError::Failed(e))
            }
        }
    }

    /// Bounded discovery for humans and agents: metrics with label keys, services, span names.
    pub async fn catalog(&self) -> Result<Catalog> {
        let since_ns = (now_ms() - 86_400_000) * 1_000_000;
        self.with_conn(move |conn| {
            let mut metrics = vec![];
            let mut stmt = conn.prepare(&format!(
                "SELECT metric_name, count(*) AS n, list_sort(list_distinct(flatten(list(map_keys(labels))))) AS keys,
                        CASE max(metric_type) WHEN 1 THEN 'counter' WHEN 2 THEN 'gauge' WHEN 3 THEN 'histogram' WHEN 4 THEN 'summary' ELSE 'unknown' END
                 FROM metrics_series GROUP BY metric_name ORDER BY metric_name LIMIT {}",
                CATALOG_LIMIT + 1
            ))?;
            let mut rows = stmt.query([])?;
            while let Some(r) = rows.next()? {
                let label_keys = text_list(r.get(2)?);
                metrics.push(CatalogMetric { name: r.get(0)?, r#type: r.get(3)?, series: r.get(1)?, label_keys, label_values: BTreeMap::new() });
            }
            // A bounded sample of values per (metric, label) for pickers.
            let mut stmt = conn.prepare(&format!(
                "SELECT metric_name, k, list_sort(list(DISTINCT v))[1:{VALUES_PER_LABEL}]
                 FROM (SELECT metric_name, unnest(map_keys(labels)) AS k, unnest(map_values(labels)) AS v FROM metrics_series)
                 WHERE v <> '' GROUP BY 1, 2"
            ))?;
            let mut rows = stmt.query([])?;
            while let Some(r) = rows.next()? {
                let (name, key): (String, String) = (r.get(0)?, r.get(1)?);
                if let Some(m) = metrics.iter_mut().find(|m| m.name == name) {
                    m.label_values.insert(key, text_list(r.get(2)?));
                }
            }
            let mut services: BTreeMap<String, CatalogService> = BTreeMap::new();
            let mut stmt = conn.prepare(
                "SELECT service_name, list_sort(list_distinct(list(span_name)))[1:50] FROM spans WHERE start_time_ns >= make_timestamp_ns(?) GROUP BY 1 ORDER BY 1 LIMIT 500",
            )?;
            let mut rows = stmt.query([since_ns])?;
            while let Some(r) = rows.next()? {
                let name: String = r.get(0)?;
                let names: duckdb::types::Value = r.get(1)?;
                let span_names = match names {
                    duckdb::types::Value::List(v) => v.into_iter().filter_map(|x| if let duckdb::types::Value::Text(t) = x { Some(t) } else { None }).collect(),
                    _ => vec![],
                };
                services.insert(name.clone(), CatalogService { name, span_names, log_lines: 0 });
            }
            let mut stmt = conn.prepare("SELECT service_name, count(*) FROM logs WHERE timestamp_ns >= make_timestamp_ns(?) GROUP BY 1 ORDER BY 1 LIMIT 500")?;
            let mut rows = stmt.query([since_ns])?;
            while let Some(r) = rows.next()? {
                let name: String = r.get(0)?;
                let n: i64 = r.get(1)?;
                services.entry(name.clone()).or_insert(CatalogService { name, span_names: vec![], log_lines: 0 }).log_lines = n;
            }
            let truncated = metrics.len() > CATALOG_LIMIT;
            metrics.truncate(CATALOG_LIMIT);
            Ok(Catalog { metrics, services: services.into_values().collect(), truncated })
        })
        .await
    }
}
