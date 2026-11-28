//! Materialized dashboard panels (SPEC §9.1, phase 1).
//!
//! A panel marked `materialize: true` keeps its computed points in a node-local DuckDB database
//! (`materialize.path`, attached as `mat`). The background loop extends them one
//! `materialize.resolution` step at a time as data becomes older than `materialize.settle`.
//! Nothing is computed for the time before a panel was first seen (no backfill).
//!
//! A sample that arrives for a timestamp at or before the stored horizon (a sender's backlog
//! after an outage, a retry) is noted by ingest, and so are another node's rows when its catalog
//! copy is attached (see `fleet.rs`); the next tick deletes the stored points from that timestamp
//! on (from the earliest stored step when it is older) and computes them again, so stored points
//! never miss accepted data. The same tick records which catalog copy of each other node the
//! stored points now include (`mat.fleet_seen`), so a restart of this node recomputes only what
//! was added since.
//!
//! A query whose document is a registered panel (ignoring `id`, `title` and `time`) and whose
//! step is a multiple of the resolution, and that overlaps the stored range, is answered on the
//! step-aligned grid plus the point at `to`: stored points for the range they cover, the normal
//! compiler for the rest. Every materializable output point depends only on its own timestamp
//! (see `tqs::materialize_blocker`), so the answer equals the raw query at those timestamps.

use crate::fleet::Seen;
use crate::node::Node;
use anyhow::Result;
use duckdb::Connection;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use tqs::compile::OutRow;

pub(crate) const DDL: &str = "
CREATE TABLE IF NOT EXISTS mat.panel_points (panel_id VARCHAR, output VARCHAR, key VARCHAR, t BIGINT, value DOUBLE);
CREATE TABLE IF NOT EXISTS mat.panel_state (panel_id VARCHAR PRIMARY KEY, name VARCHAR, panel_key VARCHAR,
    resolution_ms BIGINT, from_t BIGINT, to_t BIGINT, updated_ms BIGINT);
CREATE TABLE IF NOT EXISTS mat.fleet_seen (node_id VARCHAR PRIMARY KEY, catalog VARCHAR, snapshot_id BIGINT,
    published_at_ms BIGINT, grace_ms BIGINT);";

/// A panel being materialized.
pub struct Panel {
    /// `<dashboard id>/<query id>`, for logs and responses.
    pub name: String,
    /// Canonical document (see [`key_of`]).
    pub key: String,
    /// Stable id of (key, resolution) used in the tables.
    pub id: String,
    pub doc: tqs::Tqs,
    /// Points per step seen in the last slice (0 = not known yet): sizes the next slice.
    width: AtomicI64,
    /// When the panel was found too wide to hold `MIN_STEPS` steps within `max_points_per_panel`
    /// (0 = not): it is not stored, and measured again after `WIDE_RETRY_MS` (a cardinality spike
    /// passes).
    too_wide_at: AtomicI64,
}

/// A too-wide panel is measured again after this long.
const WIDE_RETRY_MS: i64 = 600_000;

/// Steps computed per panel per call (12 h at 15 s): bounds the work behind one query timeout.
const SLICE_STEPS: i64 = 2_880;
/// The first slice of a panel, before its width is known (15 min at 15 s).
const FIRST_SLICE_STEPS: i64 = 60;
/// A panel must be able to keep this many steps (1 h at 15 s) or it is not stored.
const MIN_STEPS: i64 = 240;

pub struct Registry {
    panels: RwLock<HashMap<String, Arc<Panel>>>,
    /// Bumped by every `set_materialized`; 0 = never registered (then nothing is forgotten).
    generation: AtomicU64,
    /// Generation whose unregistered panels were last deleted.
    cleaned: AtomicU64,
    /// Oldest timestamp of a sample ingested at or before the stored horizon; `i64::MAX` = none.
    late: AtomicI64,
    /// Catalog copies of other nodes whose rows are noted in `late` but not yet in `mat.fleet_seen`:
    /// written by the tick that rewinds for them (in the same transaction).
    fleet_seen: Mutex<HashMap<String, Seen>>,
}

impl Default for Registry {
    fn default() -> Self {
        Registry {
            panels: Default::default(),
            generation: AtomicU64::new(0),
            cleaned: AtomicU64::new(0),
            late: AtomicI64::new(i64::MAX),
            fleet_seen: Default::default(),
        }
    }
}

impl Registry {
    /// Ingest (or a fleet refresh) made a sample this old visible (at or before `now - settle`):
    /// stored steps from here on are recomputed on the next tick.
    pub(crate) fn note_late(&self, ts_ms: i64) {
        self.late.fetch_min(ts_ms, Ordering::Relaxed);
    }

    /// A fleet refresh made other nodes' copies visible: `late` as for [`Registry::note_late`],
    /// and the copies to record as included once the tick has rewound for them. One lock with the
    /// tick's, so a copy is never recorded without its rewind.
    pub(crate) fn note_fleet(&self, late: Option<i64>, seen: Vec<(String, Seen)>) {
        let mut pending = self.fleet_seen.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(t) = late {
            self.note_late(t);
        }
        pending.extend(seen);
    }

    /// What the next tick rewinds for and records: (late, copies).
    fn take_late(&self) -> (i64, Vec<(String, Seen)>) {
        let mut pending = self.fleet_seen.lock().unwrap_or_else(|e| e.into_inner());
        (self.late.swap(i64::MAX, Ordering::Relaxed), pending.drain().collect())
    }

    /// A tick that failed gives back what it took (newer copies noted meanwhile win).
    fn give_back(&self, late: i64, seen: Vec<(String, Seen)>) {
        let mut pending = self.fleet_seen.lock().unwrap_or_else(|e| e.into_inner());
        self.note_late(late);
        for (node, s) in seen {
            pending.entry(node).or_insert(s);
        }
    }
}

/// What one background slice of one panel did.
#[derive(Default)]
struct Slice {
    stored: usize,
    more: bool,
    dropped: u64,
    width: i64,
    wide: bool,
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

/// Saturating at the ends of `i64`: a noted time may be "before everything" (`i64::MIN`).
fn ceil_to(x: i64, step: i64) -> i64 {
    let f = floor_to(x, step);
    if f == x { x } else { f.saturating_add(step) }
}

fn floor_to(x: i64, step: i64) -> i64 {
    x.div_euclid(step).saturating_mul(step)
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
        let mut panels = panels;
        panels.sort_by(|a, b| a.0.cmp(&b.0)); // the same panels win on every start
        let max = self.cfg.materialize.max_panels;
        let mut map = HashMap::new();
        let mut over = 0usize;
        for (name, json) in panels {
            let Ok(doc) = tqs::Tqs::parse(&json.to_string()) else { continue };
            if tqs::materialize_blocker(&doc).is_some() {
                continue;
            }
            let key = key_of(&json);
            if map.len() >= max && !map.contains_key(&key) {
                over += 1;
                tracing::warn!(panel = %name, max_panels = max, "materialize.max_panels reached: panel not materialized");
                continue;
            }
            // The engine version is in the id: stored points never outlive a compiler change.
            let id = fnv1a(&format!("{}:{res}:{key}", env!("CARGO_PKG_VERSION")));
            // Keep what was learned about a panel that is registered again unchanged.
            let old = self.mat.panels.read().ok().and_then(|m| m.get(&key).cloned());
            let (width, wide_at) = old.map(|o| (o.width.load(Ordering::Relaxed), o.too_wide_at.load(Ordering::Relaxed))).unwrap_or((0, 0));
            map.insert(
                key.clone(),
                Arc::new(Panel { name, key, id, doc, width: AtomicI64::new(width), too_wide_at: AtomicI64::new(wide_at) }),
            );
        }
        tracing::info!(panels = map.len(), skipped = over, "materialized panels registered");
        self.metrics.materialized_panels.store(map.len() as u64, Ordering::Relaxed);
        *self.mat.panels.write().unwrap_or_else(|e| e.into_inner()) = map;
        self.mat.generation.fetch_add(1, Ordering::Relaxed);
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

    /// Forget unregistered panels and rewind past late samples, then compute one slice (at most
    /// `SLICE_STEPS`) per panel, each on its own connection so one slow or failing panel does
    /// not hold back the others. `.1` is true when a panel still lags behind the horizon.
    async fn extend(&self, start: Option<i64>, now_ms: i64) -> Result<(usize, bool)> {
        let res = self.cfg.materialize.resolution.ms();
        let horizon = floor_to(now_ms - self.cfg.materialize.settle.ms(), res);
        // The generation first: a registration in between makes this tick clean up against the older
        // list under the older generation, so the next tick cleans up again.
        let generation = self.mat.generation.load(Ordering::Relaxed);
        let panels: Vec<Arc<Panel>> = self.mat.panels.read().map(|m| m.values().cloned().collect()).unwrap_or_default();
        let cleanup = generation > 0 && generation != self.mat.cleaned.load(Ordering::Relaxed);
        // Before the first registration nothing is forgotten or rewound; the note waits (a restart
        // replays the WAL and notes its samples before the dashboards are loaded).
        let (late, seen) = if generation == 0 { (i64::MAX, vec![]) } else { self.mat.take_late() };
        if cleanup || late != i64::MAX || !seen.is_empty() {
            let keep = panels.iter().map(|p| format!("'{}'", p.id)).collect::<Vec<_>>().join(", ");
            let seen_back = seen.clone();
            let r = self
                .with_conn(move |conn| {
                    conn.execute_batch("BEGIN TRANSACTION")?;
                    let done = (|| -> Result<()> {
                        if cleanup {
                            // Panels that left every dashboard, or whose query changed. Ids are hex.
                            let gone = if keep.is_empty() { "true".to_string() } else { format!("panel_id NOT IN ({keep})") };
                            conn.execute_batch(&format!(
                                "DELETE FROM mat.panel_state WHERE {gone}; DELETE FROM mat.panel_points WHERE {gone};"
                            ))?;
                        }
                        // Nothing stored: nothing to rewind.
                        let earliest: Option<i64> = conn.query_row("SELECT min(from_t) FROM mat.panel_state", [], |r| r.get(0))?;
                        if late != i64::MAX
                            && let Some(earliest) = earliest
                        {
                            // Steps at or after the late sample may have changed: drop them and end
                            // the coverage just before the first one (empty when that is before
                            // from_t). A time before every stored step counts from the earliest.
                            let first = ceil_to(late.max(earliest), res);
                            conn.execute("DELETE FROM mat.panel_points WHERE t >= ?", [first])?;
                            conn.execute(
                                "UPDATE mat.panel_state SET to_t = greatest(from_t, ?) - ? WHERE to_t >= ?",
                                duckdb::params![first, res, first],
                            )?;
                        }
                        for (node, s) in &seen {
                            conn.execute(
                                "INSERT INTO mat.fleet_seen VALUES (?, ?, ?, ?, ?) ON CONFLICT (node_id) DO UPDATE SET catalog = excluded.catalog,
                                 snapshot_id = excluded.snapshot_id, published_at_ms = excluded.published_at_ms, grace_ms = excluded.grace_ms",
                                duckdb::params![node, s.catalog, s.snapshot_id, s.published_at_ms, s.grace_ms],
                            )?;
                        }
                        Ok(())
                    })();
                    match done {
                        Ok(()) => Ok(conn.execute_batch("COMMIT")?),
                        Err(e) => {
                            let _ = conn.execute_batch("ROLLBACK");
                            Err(e)
                        }
                    }
                })
                .await;
            match r {
                Ok(()) if cleanup => self.mat.cleaned.store(generation, Ordering::Relaxed),
                Ok(()) => {}
                Err(e) => {
                    self.mat.give_back(late, seen_back); // retry on the next tick
                    return Err(e.context("materialized panels: forgetting or rewinding"));
                }
            }
        }
        let (mut stored, mut more) = (0usize, false);
        let (max_points, max_slice) = (self.cfg.materialize.max_points_per_panel as i64, self.cfg.materialize.max_slice_points as i64);
        let mut trimmed_total = 0u64;
        for p in panels {
            let wide_at = p.too_wide_at.load(Ordering::Relaxed);
            if wide_at != 0 && now_ms < wide_at + WIDE_RETRY_MS {
                continue;
            }
            let name = p.name.clone();
            let pp = p.clone();
            let r = self
                .with_conn(move |conn| {
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
                        return Ok(Slice::default());
                    }
                    // A slice is sized by the panel's width, so one statement builds about
                    // `max_slice_points` output rows however many series the panel has.
                    let width = p.width.load(Ordering::Relaxed);
                    let steps = if width <= 0 { FIRST_SLICE_STEPS } else { (max_slice / width).clamp(1, SLICE_STEPS) };
                    let hi = horizon.min(lo + res * (steps - 1));
                    // Compute and store in one statement and one transaction.
                    let (sql, params) = tqs::compile::timeseries_sql(&p.doc, &tqs::Resolved { from_ms: lo, to_ms: hi, step_ms: res })?;
                    // The compiled SQL numbers its parameters ($1, $2, …); the id is 16 hex digits,
                    // so it goes in as a literal.
                    debug_assert!(p.id.bytes().all(|b| b.is_ascii_hexdigit()));
                    conn.execute_batch("BEGIN TRANSACTION")?;
                    let written = (|| -> Result<Slice> {
                        let n = conn.execute(
                            &format!("INSERT INTO mat.panel_points SELECT '{}', output, key, t, value FROM ({sql})", p.id),
                            duckdb::params_from_iter(params.iter()),
                        )?;
                        let done_steps = (hi - lo) / res + 1;
                        let w = (n as i64 + done_steps - 1) / done_steps;
                        let more = hi < horizon;
                        // A panel too wide to keep MIN_STEPS steps is not stored at all.
                        if w * MIN_STEPS > max_points {
                            conn.execute("DELETE FROM mat.panel_points WHERE panel_id = ?", [&p.id])?;
                            conn.execute("DELETE FROM mat.panel_state WHERE panel_id = ?", [&p.id])?;
                            return Ok(Slice { width: w, wide: true, ..Slice::default() });
                        }
                        conn.execute(
                            "INSERT INTO mat.panel_state VALUES (?, ?, ?, ?, ?, ?, ?)
                             ON CONFLICT (panel_id) DO UPDATE SET to_t = excluded.to_t, updated_ms = excluded.updated_ms",
                            duckdb::params![p.id, p.name, p.key, res, lo, hi, now_ms],
                        )?;
                        // Keep about the newest `max_points` points: drop the oldest whole steps.
                        // The cut comes from the width, so no count or sort of the store is needed;
                        // `t` is sorted by insertion, so the delete only touches old row groups.
                        let keep_from = hi - (max_points / w.max(1)).max(MIN_STEPS).saturating_sub(1) * res;
                        let from_t: i64 = conn.query_row("SELECT from_t FROM mat.panel_state WHERE panel_id = ?", [&p.id], |r| r.get(0))?;
                        let mut dropped = 0;
                        if from_t < keep_from {
                            dropped = conn
                                .execute("DELETE FROM mat.panel_points WHERE panel_id = ? AND t < ?", duckdb::params![p.id, keep_from])?;
                            conn.execute("UPDATE mat.panel_state SET from_t = ? WHERE panel_id = ?", duckdb::params![keep_from, p.id])?;
                        }
                        Ok(Slice { stored: n, more, dropped: dropped as u64, width: w, wide: false })
                    })();
                    match written {
                        Ok(sl) => {
                            conn.execute_batch("COMMIT")?;
                            Ok(sl)
                        }
                        Err(e) => {
                            let _ = conn.execute_batch("ROLLBACK");
                            Err(e)
                        }
                    }
                })
                .await;
            match r {
                Ok(sl) => {
                    // Side effects only once the slice is committed.
                    if sl.width > 0 {
                        pp.width.store(sl.width, Ordering::Relaxed);
                    }
                    if sl.wide {
                        pp.too_wide_at.store(now_ms.max(1), Ordering::Relaxed);
                        tracing::warn!(panel = %name, points_per_step = sl.width, max_points_per_panel = max_points,
                            "materialized panel is too wide (series per step) to be stored; reads compute it raw, measured again in 10 min");
                    } else {
                        pp.too_wide_at.store(0, Ordering::Relaxed);
                    }
                    stored += sl.stored;
                    more |= sl.more;
                    trimmed_total += sl.dropped;
                }
                Err(e) => {
                    // A slice that fails (a timeout, say) is retried half as long.
                    let w = pp.width.load(Ordering::Relaxed).max(1);
                    pp.width.store(w.saturating_mul(2), Ordering::Relaxed);
                    tracing::warn!(panel = %name, error = %format!("{e:#}"), "materializing a panel failed; will retry with a shorter slice");
                }
            }
        }
        self.metrics.materialized_trimmed.fetch_add(trimmed_total, Ordering::Relaxed);
        if let Ok(total) = self.with_conn(|c| Ok(c.query_row("SELECT count(*) FROM mat.panel_points", [], |r| r.get::<_, i64>(0))?)).await {
            self.metrics.materialized_stored.store(total.max(0) as u64, Ordering::Relaxed);
        }
        self.metrics.materialized_points.fetch_add(stored as u64, Ordering::Relaxed);
        Ok((stored, more))
    }

    /// Delete stored points older than `cutoff_ms` (retention).
    pub(crate) async fn retain_materialized(&self, cutoff_ms: i64) -> Result<()> {
        // On the step grid, so a rewind past `from_t` later stays aligned.
        let cutoff_ms = ceil_to(cutoff_ms, self.cfg.materialize.resolution.ms().max(1));
        self.with_conn(move |conn| {
            // State first, in one transaction: a reader never sees coverage over deleted points.
            conn.execute_batch("BEGIN TRANSACTION")?;
            let done = conn
                .execute("UPDATE mat.panel_state SET from_t = ? WHERE from_t < ?", [cutoff_ms, cutoff_ms])
                .and_then(|_| conn.execute("DELETE FROM mat.panel_points WHERE t < ?", [cutoff_ms]));
            match done {
                Ok(_) => Ok(conn.execute_batch("COMMIT")?),
                Err(e) => {
                    let _ = conn.execute_batch("ROLLBACK");
                    Err(e.into())
                }
            }
        })
        .await
    }
}

/// Answer a timeseries query for a materialized panel on the step-aligned grid inside `[from, to]`
/// plus the point at `to`: stored points where they exist, the compiler for the head and tail
/// they do not cover. `None` when no stored point falls in the range (the caller runs the raw
/// query unchanged).
pub(crate) fn read(conn: &Connection, p: &Panel, doc: &tqs::Tqs, r: &tqs::Resolved, res: i64) -> Result<Option<(Vec<OutRow>, Info)>> {
    let s = r.step_ms;
    let (from, to) = (ceil_to(r.from_ms, s), floor_to(r.to_ms, s));
    if from > to {
        return Ok(None);
    }
    let state: Option<(i64, i64)> = conn
        .query_row("SELECT from_t, to_t FROM mat.panel_state WHERE panel_id = ?", [&p.id], |row| Ok((row.get(0)?, row.get(1)?)))
        .map(Some)
        .or_else(|e| if matches!(e, duckdb::Error::QueryReturnedNoRows) { Ok(None) } else { Err(e) })?;
    let Some((c0, c1)) = state.map(|(a, b)| (from.max(ceil_to(a, s)), to.min(floor_to(b, s)))).filter(|(a, b)| a <= b) else {
        return Ok(None);
    };
    let mut info = Info { panel: p.name.clone(), resolution_ms: res, stored_points: 0, computed_steps: 0 };
    let mut stmt =
        conn.prepare("SELECT output, t, key, value FROM mat.panel_points WHERE panel_id = ? AND t BETWEEN ? AND ? AND t % ? = 0")?;
    let mut rows = stmt
        .query_map(duckdb::params![p.id, c0, c1, s], |row| {
            Ok(OutRow { output: row.get(0)?, t: row.get(1)?, key: row.get(2)?, value: row.get(3)? })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if rows.is_empty() {
        return Ok(None); // coverage without a point (no data): the caller runs the raw query
    }
    info.stored_points = rows.len();
    if from < c0 {
        rows.extend(run(conn, doc, from, c0 - s, s)?);
        info.computed_steps += (c0 - from) / s;
    }
    if c1 < to {
        rows.extend(run(conn, doc, c1 + s, to, s)?);
        info.computed_steps += (to - c1) / s;
    }
    // The newest point, as the raw query would end: dashboards stay as fresh as without the store.
    if r.to_ms > to {
        rows.extend(run(conn, doc, r.to_ms, r.to_ms, s)?);
        info.computed_steps += 1;
    }
    rows.sort_by(|a, b| (&a.output, &a.key, a.t).cmp(&(&b.output, &b.key, b.t)));
    Ok(Some((rows, info)))
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
        // "Before everything" stays representable instead of overflowing.
        assert_eq!(ceil_to(i64::MIN, 15_000), i64::MIN);
        assert_eq!(floor_to(i64::MIN, 15_000), i64::MIN);
        assert_eq!(ceil_to(-15_001, 15_000), -15_000);
        assert_eq!(fnv1a("x"), fnv1a("x"));
        assert_ne!(fnv1a("x"), fnv1a("y"));
    }
}
