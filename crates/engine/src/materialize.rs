//! Materialized dashboard panels (SPEC §9.1, phase 1).
//!
//! A panel marked `materialize: true` keeps its computed points in a node-local DuckDB database
//! (`materialize.path`, attached as `mat`). The background loop extends them one
//! `materialize.resolution` step at a time as data becomes older than `materialize.settle`.
//! Nothing is computed for the time before a panel was first seen (no backfill).
//!
//! A query whose document is a registered panel (ignoring `id`, `title` and `time`) and whose
//! step is a multiple of the resolution is answered on the step-aligned grid: stored points for
//! the range they cover, the normal compiler for the rest. Every materializable output point
//! depends only on its own timestamp (see `tqs::materialize_blocker`), so the answer equals the
//! raw query on the same grid.

use crate::node::Node;
use anyhow::Result;
use duckdb::Connection;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tqs::compile::OutRow;

pub(crate) const DDL: &str = "
CREATE TABLE IF NOT EXISTS mat.panel_points (panel_id VARCHAR, output VARCHAR, key VARCHAR, t BIGINT, value DOUBLE);
CREATE TABLE IF NOT EXISTS mat.panel_state (panel_id VARCHAR PRIMARY KEY, name VARCHAR, panel_key VARCHAR,
    resolution_ms BIGINT, from_t BIGINT, to_t BIGINT, updated_ms BIGINT);";

/// A panel being materialized.
pub struct Panel {
    /// `<dashboard id>/<query id>`, for logs and responses.
    pub name: String,
    /// Canonical document (see [`key_of`]).
    pub key: String,
    /// Stable id of (key, resolution) used in the tables.
    pub id: String,
    pub doc: tqs::Tqs,
}

#[derive(Default)]
pub struct Registry {
    panels: RwLock<HashMap<String, Arc<Panel>>>,
}

/// How a query was answered when stored points were used.
#[derive(Debug, Clone, Serialize)]
pub struct Info {
    pub panel: String,
    pub resolution_ms: i64,
    pub stored_points: usize,
    pub computed_steps: i64,
}

/// The identity of a query for materialization: its JSON without `id`, `title`, `time` and empty
/// `vars`, with object keys sorted, so the UI's copy and the dashboard file's copy agree.
pub fn key_of(doc: &serde_json::Value) -> String {
    fn canon(v: &serde_json::Value) -> serde_json::Value {
        match v {
            serde_json::Value::Object(m) => {
                let mut keys: Vec<&String> = m.keys().collect();
                keys.sort();
                serde_json::Value::Object(keys.into_iter().map(|k| (k.clone(), canon(&m[k]))).collect())
            }
            serde_json::Value::Array(a) => serde_json::Value::Array(a.iter().map(canon).collect()),
            other => other.clone(),
        }
    }
    let mut v = doc.clone();
    if let Some(m) = v.as_object_mut() {
        for k in ["id", "title", "time"] {
            m.remove(k);
        }
        if m.get("vars").and_then(|x| x.as_object()).is_some_and(|o| o.is_empty()) {
            m.remove("vars");
        }
    }
    canon(&v).to_string()
}

/// FNV-1a 64: stable across builds, so stored points survive a restart.
fn fnv1a(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

fn ceil_to(x: i64, step: i64) -> i64 {
    x.div_euclid(step) * step + if x.rem_euclid(step) == 0 { 0 } else { step }
}

fn floor_to(x: i64, step: i64) -> i64 {
    x.div_euclid(step) * step
}

fn run(conn: &Connection, doc: &tqs::Tqs, from_ms: i64, to_ms: i64, step_ms: i64) -> Result<Vec<OutRow>> {
    tqs::compile::run(conn, doc, &tqs::Resolved { from_ms, to_ms, step_ms })
}

impl Node {
    /// Replace the set of materialized panels: `(name, effective query document)`. Documents that
    /// cannot be materialized are skipped (dashboard validation reports them). Points of panels
    /// that are gone are deleted on the next [`Node::materialize`].
    pub fn set_materialized(&self, panels: Vec<(String, serde_json::Value)>) {
        let res = self.cfg.materialize.resolution.ms();
        let mut map = HashMap::new();
        for (name, json) in panels {
            let Ok(doc) = tqs::Tqs::parse(&json.to_string()) else { continue };
            if tqs::materialize_blocker(&doc).is_some() {
                continue;
            }
            let key = key_of(&json);
            let id = fnv1a(&format!("{res}:{key}"));
            map.insert(key.clone(), Arc::new(Panel { name, key, id, doc }));
        }
        tracing::info!(panels = map.len(), "materialized panels registered");
        *self.mat.panels.write().unwrap_or_else(|e| e.into_inner()) = map;
    }

    /// The registered panel a query document is, if its step can use the stored points.
    pub(crate) fn materialized_panel(&self, json: &str, step_ms: i64) -> Option<Arc<Panel>> {
        let res = self.cfg.materialize.resolution.ms();
        if res <= 0 || step_ms % res != 0 {
            return None;
        }
        let panels = self.mat.panels.read().ok()?;
        if panels.is_empty() {
            return None;
        }
        let v: serde_json::Value = serde_json::from_str(json).ok()?;
        panels.get(&key_of(&v)).cloned()
    }

    /// Extend every registered panel up to `now - settle`. Returns the points stored.
    pub async fn materialize(&self, now_ms: i64) -> Result<usize> {
        self.materialize_from(None, now_ms).await
    }

    /// Like [`Node::materialize`], but a panel with nothing stored yet starts at `from_ms`
    /// instead of at the settle horizon, and the call returns only once every panel is caught
    /// up (benchmarks and tests; the background loop never backfills).
    pub async fn materialize_since(&self, from_ms: i64, now_ms: i64) -> Result<usize> {
        let mut total = 0;
        loop {
            let (n, more) = self.extend(Some(from_ms), now_ms).await?;
            total += n;
            if !more {
                return Ok(total);
            }
        }
    }

    async fn materialize_from(&self, start: Option<i64>, now_ms: i64) -> Result<usize> {
        Ok(self.extend(start, now_ms).await?.0)
    }

    /// One slice (at most `tqs::MAX_POINTS` steps) per panel; `.1` is true when a panel still
    /// lags behind the horizon.
    async fn extend(&self, start: Option<i64>, now_ms: i64) -> Result<(usize, bool)> {
        let res = self.cfg.materialize.resolution.ms();
        let horizon = floor_to(now_ms - self.cfg.materialize.settle.ms(), res);
        let panels: Vec<Arc<Panel>> = self.mat.panels.read().map(|m| m.values().cloned().collect()).unwrap_or_default();
        let stored = self
            .with_conn(move |conn| {
                // Forget panels that are no longer on any dashboard (or whose query changed).
                let keep = panels.iter().map(|p| format!("'{}'", p.id)).collect::<Vec<_>>().join(", ");
                let gone = if keep.is_empty() { "true".to_string() } else { format!("panel_id NOT IN ({keep})") };
                conn.execute_batch(&format!("DELETE FROM mat.panel_points WHERE {gone}; DELETE FROM mat.panel_state WHERE {gone};"))?;
                let (mut stored, mut more) = (0usize, false);
                for p in &panels {
                    let to_t: Option<i64> = conn
                        .query_row("SELECT to_t FROM mat.panel_state WHERE panel_id = ?", [&p.id], |r| r.get(0))
                        .map(Some)
                        .or_else(|e| if matches!(e, duckdb::Error::QueryReturnedNoRows) { Ok(None) } else { Err(e) })?;
                    let lo = match (to_t, start) {
                        (Some(t), _) => t + res,
                        (None, Some(s)) => ceil_to(s, res),
                        (None, None) => horizon,
                    };
                    if lo > horizon {
                        continue;
                    }
                    let hi = horizon.min(lo + res * (tqs::MAX_POINTS - 1));
                    more |= hi < horizon;
                    let rows = run(conn, &p.doc, lo, hi, res)?;
                    conn.execute_batch("BEGIN TRANSACTION")?;
                    let written = (|| -> Result<usize> {
                        let mut ins = conn.prepare("INSERT INTO mat.panel_points VALUES (?, ?, ?, ?, ?)")?;
                        for r in &rows {
                            ins.execute(duckdb::params![p.id, r.output, r.key, r.t, r.value])?;
                        }
                        conn.execute(
                            "INSERT INTO mat.panel_state VALUES (?, ?, ?, ?, ?, ?, ?)
                             ON CONFLICT (panel_id) DO UPDATE SET to_t = excluded.to_t, updated_ms = excluded.updated_ms",
                            duckdb::params![p.id, p.name, p.key, res, lo, hi, now_ms],
                        )?;
                        Ok(rows.len())
                    })();
                    match written {
                        Ok(n) => {
                            conn.execute_batch("COMMIT")?;
                            stored += n;
                        }
                        Err(e) => {
                            let _ = conn.execute_batch("ROLLBACK");
                            return Err(e.context(format!("materializing {}", p.name)));
                        }
                    }
                }
                Ok((stored, more))
            })
            .await?;
        self.metrics.materialized_points.fetch_add(stored.0 as u64, std::sync::atomic::Ordering::Relaxed);
        Ok(stored)
    }

    /// Delete stored points older than `cutoff_ms` (retention).
    pub(crate) async fn retain_materialized(&self, cutoff_ms: i64) -> Result<()> {
        self.with_conn(move |conn| {
            conn.execute("DELETE FROM mat.panel_points WHERE t < ?", [cutoff_ms])?;
            conn.execute("UPDATE mat.panel_state SET from_t = greatest(from_t, ?)", [cutoff_ms])?;
            Ok(())
        })
        .await
    }
}

/// Answer a timeseries query for a materialized panel on the step-aligned grid inside `[from, to]`:
/// stored points where they exist, the compiler for the head and tail they do not cover.
pub(crate) fn read(conn: &Connection, p: &Panel, doc: &tqs::Tqs, r: &tqs::Resolved, res: i64) -> Result<(Vec<OutRow>, Info)> {
    let s = r.step_ms;
    let (from, to) = (ceil_to(r.from_ms, s), floor_to(r.to_ms, s));
    let mut info = Info { panel: p.name.clone(), resolution_ms: res, stored_points: 0, computed_steps: 0 };
    if from > to {
        return Ok((vec![], info));
    }
    let state: Option<(i64, i64)> = conn
        .query_row("SELECT from_t, to_t FROM mat.panel_state WHERE panel_id = ?", [&p.id], |row| Ok((row.get(0)?, row.get(1)?)))
        .map(Some)
        .or_else(|e| if matches!(e, duckdb::Error::QueryReturnedNoRows) { Ok(None) } else { Err(e) })?;
    let covered = state.map(|(a, b)| (from.max(ceil_to(a, s)), to.min(floor_to(b, s)))).filter(|(a, b)| a <= b);
    let mut rows = vec![];
    match covered {
        None => {
            rows = run(conn, doc, from, to, s)?;
            info.computed_steps = (to - from) / s + 1;
        }
        Some((c0, c1)) => {
            let mut stmt =
                conn.prepare("SELECT output, t, key, value FROM mat.panel_points WHERE panel_id = ? AND t BETWEEN ? AND ? AND t % ? = 0")?;
            let stored = stmt
                .query_map(duckdb::params![p.id, c0, c1, s], |row| {
                    Ok(OutRow { output: row.get(0)?, t: row.get(1)?, key: row.get(2)?, value: row.get(3)? })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            info.stored_points = stored.len();
            rows.extend(stored);
            if from < c0 {
                rows.extend(run(conn, doc, from, c0 - s, s)?);
                info.computed_steps += (c0 - from) / s;
            }
            if c1 < to {
                rows.extend(run(conn, doc, c1 + s, to, s)?);
                info.computed_steps += (to - c1) / s;
            }
        }
    }
    rows.sort_by(|a, b| (&a.output, &a.key, a.t).cmp(&(&b.output, &b.key, b.t)));
    Ok((rows, info))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_ignore_identity_time_and_key_order() {
        let a = serde_json::json!({"tqs": "3", "id": "a", "title": "A", "kind": "timeseries", "time": {"from": "-1h"},
            "queries": [{"name": "r", "metric": "m", "fn": {"op": "rate", "window": "5m"}}], "vars": {}});
        let b = serde_json::json!({"queries": [{"fn": {"window": "5m", "op": "rate"}, "metric": "m", "name": "r"}],
            "kind": "timeseries", "tqs": "3", "id": "b", "time": {"from": "-6h", "step": "2m"}});
        assert_eq!(key_of(&a), key_of(&b));
        let c = serde_json::json!({"tqs": "3", "kind": "timeseries", "queries": [{"name": "r", "metric": "m", "fn": {"op": "rate", "window": "1m"}}]});
        assert_ne!(key_of(&a), key_of(&c));
    }

    #[test]
    fn grid_helpers() {
        assert_eq!(ceil_to(31, 15), 45);
        assert_eq!(ceil_to(30, 15), 30);
        assert_eq!(floor_to(44, 15), 30);
        assert_eq!(fnv1a("x"), fnv1a("x"));
        assert_ne!(fnv1a("x"), fnv1a("y"));
    }
}
