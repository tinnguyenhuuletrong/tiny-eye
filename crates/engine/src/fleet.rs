//! A fleet: nodes that share `storage.url` find each other there and answer queries over all
//! their data. No peers list, no coordinator, no shared catalog: every node is a full node with
//! its own catalog file and its own object prefix `<region>/<id>/`.
//!
//! Publish (every `fleet.publish_interval`, under the catalog lock so it never overlaps a
//! checkpoint): copy the DuckLake metadata into a fresh DuckDB file, gzip it to
//! `<region>/<id>/_fleet/catalog.ducklake.gz`, then write the manifest `_fleet/<id>.json`. When
//! two processes publish under one node id, the one started last wins: the older one fails every
//! publish (logged, counted in `fleet_errors`).
//!
//! Refresh (every `fleet.refresh_interval`): list `_fleet/*.json`, skip this node, manifests not
//! written for longer than `coordinator.snapshot_grace` (not even fetched) and copies too old to be
//! safe (below), download the copies that changed, check them, attach them read-only under new
//! aliases and swap the query views to own lake ∪ own hot tail ∪ every other node's tables.
//! Replaced aliases are detached once a query that started before the swap can no longer be
//! running (`query.timeout`).
//!
//! Copies are not trusted: anyone who can write to the shared storage can write one. Before a copy
//! reaches the views its manifest must name itself (`_fleet/<id>.json`, catalog key under
//! `<region>/<id>/`), it must unpack to at most `fleet.max_copy_bytes`, its metadata database must
//! hold no views, macros or generated columns (DuckDB would run them), its DuckLake catalog no
//! views or macros, exactly this node's tables and columns, and only data files under the
//! publisher's own prefix. A refused copy is detached, deleted, logged and counted once per
//! (node, snapshot), and the node's previous copy is kept while it is fresh.
//!
//! Safe deletion: retention expires a snapshot only once a newer one has existed for the
//! publisher's `snapshot_grace` (see `Node::retain_lake`). A reader drops a copy older than
//! `min(own grace, publisher's grace) - refresh_interval - query.timeout`: it reads a copy for at
//! most one more refresh interval plus a query timeout, so it never reads past the grace.
//!
//! Materialized panels: rows of another node become visible here when its copy is swapped in. The
//! earliest sample time among them comes from the copy's own file statistics (DuckLake keeps
//! min/max per file): files added since the copy this node read before, or every file when the
//! node is new here (first seen, back after being dropped, or restarted with a new epoch). If
//! that time is at or before the settle horizon, it is noted as late, so stored steps are
//! recomputed exactly as for a late local sample.
//!
//! No label is added per node or region: like Prometheus external labels, senders label their
//! region. Series ids include the receiving node's region, so one label set sent to two regions
//! stays two series (aggregations add them up); sent to two nodes of one region it is one series.

use crate::config::check_id;
use crate::node::{Node, now_ms, sql_str};
use crate::tables::Table;
use anyhow::{Context, Result, anyhow, bail};
use duckdb::Connection;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Instant;

/// Version of the manifest and catalog copy; a node skips manifests of another format.
pub const FORMAT: u32 = 2;
/// DuckLake keeps the metadata of the catalog attached as `lake` in this database. It is an
/// internal name (spec Q19): `tests/fleet.rs` fails if a DuckDB/DuckLake upgrade changes it.
pub const METADATA_DB: &str = "__ducklake_metadata_lake";
/// The gzipped catalog copy, relative to the node's own prefix.
pub const CATALOG_KEY: &str = "_fleet/catalog.ducklake.gz";

/// `_fleet/<node_id>.json` at the root of `storage.url`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Manifest {
    pub format: u32,
    pub node_id: String,
    pub region: String,
    /// Start time (ms) of the publishing process: tells two processes using one node id apart.
    pub epoch: i64,
    /// The publisher's lake snapshot the catalog copy holds.
    pub snapshot_id: i64,
    pub published_at_ms: i64,
    /// The publisher's `coordinator.snapshot_grace`: files the copy names outlive its snapshot
    /// being current by this long.
    pub snapshot_grace_ms: i64,
    /// Key of the gzipped catalog copy, relative to `storage.url`: always
    /// `<region>/<node_id>/_fleet/catalog.ducklake.gz`.
    pub catalog_key: String,
}

/// What every manifest format has: read first, so another format is skipped by its number.
#[derive(Deserialize)]
struct Head {
    format: u32,
    #[serde(default)]
    epoch: i64,
}

pub fn manifest_key(node_id: &str) -> String {
    format!("_fleet/{node_id}.json")
}

pub fn catalog_key(region: &str, node_id: &str) -> String {
    format!("{region}/{node_id}/{CATALOG_KEY}")
}

/// A catalog copy refused for what it holds (not a passing failure): never queried, and not
/// downloaded again for the same (node, epoch, snapshot).
#[derive(Debug)]
pub struct Rejected(pub String);

impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Rejected {}

fn rejected(why: String) -> anyhow::Error {
    anyhow::Error::new(Rejected(why))
}

#[derive(Clone, Debug)]
struct Attached {
    m: Manifest,
    alias: String,
    path: PathBuf,
}

/// What this node has attached of the others (guarded by one async lock: refreshes are serial).
#[derive(Default)]
pub(crate) struct State {
    next_alias: u64,
    attached: Vec<Attached>,
    /// Aliases no view names any more, with the time from which they may be detached.
    retired: Vec<(String, PathBuf, Instant)>,
    /// Problems already logged: (manifest key or node id, what).
    warned: HashSet<(String, String)>,
    /// Refused copies and manifests: (manifest key, epoch, snapshot id).
    rejected: HashSet<(String, i64, i64)>,
}

impl Node {
    /// Local directory of this node's downloaded copies (`fleet.dir/<node.id>`).
    pub(crate) fn fleet_dir(&self) -> PathBuf {
        Path::new(&self.cfg.fleet.dir).join(&self.cfg.node.id)
    }

    /// Publish this node's catalog for the others: the copy (only when the snapshot changed since
    /// this process last published), then the manifest pointing at it. A failure is counted in
    /// `fleet_errors`.
    pub async fn publish(&self) -> Result<Manifest> {
        let r = self.publish_copy().await;
        if r.is_err() {
            self.metrics.fleet_errors.fetch_add(1, Ordering::Relaxed);
        }
        r
    }

    async fn publish_copy(&self) -> Result<Manifest> {
        let _g = self.catalog_lock.lock().await;
        let id = &self.cfg.node.id;
        let mkey = manifest_key(id);
        let mut last = *self.published.lock().map_err(|_| anyhow!("publish lock poisoned"))?;
        // Another epoch in our manifest: another process publishes under our node id (and shares
        // our prefix in this region). The newest process wins; the older one is the stale one.
        if let Ok(b) = self.shared.get(&mkey).await
            && let Ok(m) = serde_json::from_slice::<Head>(&b)
            && m.epoch != self.epoch
        {
            if m.epoch > self.epoch {
                bail!(
                    "node.id {id:?} is used by another node too: {mkey} was written by a process started later (at {}, this one \
                     started at {}), so that one publishes and this one does not; every node sharing storage.url needs its own node.id",
                    m.epoch,
                    self.epoch
                );
            }
            // An older process may have replaced our copy too: upload ours again.
            last = None;
        }
        let conn = self.lake_conn()?;
        let path = self.fleet_dir().join(format!("publish-{}.duckdb", ulid::Ulid::generate()));
        let (snapshot_id, gz) = tokio::task::spawn_blocking(move || -> Result<(i64, Option<Vec<u8>>)> {
            let snap: i64 = conn.query_row("SELECT max(snapshot_id) FROM ducklake_snapshots('lake')", [], |r| r.get(0))?;
            if last == Some(snap) {
                return Ok((snap, None));
            }
            let res = conn.execute_batch(&format!(
                "ATTACH {} AS fleet_publish; COPY FROM DATABASE {METADATA_DB} TO fleet_publish; DETACH fleet_publish;",
                sql_str(&path.display().to_string())
            ));
            if res.is_err() {
                let _ = conn.execute_batch("DETACH DATABASE IF EXISTS fleet_publish");
            }
            let bytes = res.context("copying the catalog metadata").and_then(|_| Ok(std::fs::read(&path)?));
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(format!("{}.wal", path.display()));
            let bytes = bytes?;
            // The file is mostly empty blocks: ~5 MB gzips to ~50 KB.
            let mut gz = flate2::write::GzEncoder::new(Vec::with_capacity(bytes.len() / 64), flate2::Compression::fast());
            gz.write_all(&bytes)?;
            Ok((snap, Some(gz.finish()?)))
        })
        .await??;
        if let Some(gz) = gz {
            self.store.put(CATALOG_KEY, gz.into()).await?;
        }
        let m = Manifest {
            format: FORMAT,
            node_id: id.clone(),
            region: self.cfg.node.region.clone(),
            epoch: self.epoch,
            snapshot_id,
            published_at_ms: now_ms(),
            snapshot_grace_ms: self.cfg.coordinator.snapshot_grace.ms(),
            catalog_key: catalog_key(&self.cfg.node.region, id),
        };
        self.shared.put(&mkey, serde_json::to_vec(&m)?.into()).await?;
        *self.published.lock().map_err(|_| anyhow!("publish lock poisoned"))? = Some(snapshot_id);
        self.metrics.fleet_publishes.fetch_add(1, Ordering::Relaxed);
        Ok(m)
    }

    /// A copy published longer ago than this (ms) may name files its publisher has deleted by the
    /// time this node stops reading it (one more refresh interval plus a query timeout).
    fn stale_after(&self, m: &Manifest) -> i64 {
        m.snapshot_grace_ms.min(self.cfg.coordinator.snapshot_grace.ms())
            - self.cfg.fleet.refresh_interval.0.as_millis() as i64
            - self.cfg.query.timeout.ms()
    }

    /// Attach the current catalog copies of the other nodes and swap the query views to them.
    /// `now` decides which manifests are stale. Returns how many other nodes queries now read. A
    /// refresh that fails or leaves out a copy counts once in `fleet_errors`.
    pub async fn refresh_fleet(&self, now: i64) -> Result<usize> {
        let r = self.refresh(now).await;
        if !matches!(r, Ok((_, false))) {
            self.metrics.fleet_errors.fetch_add(1, Ordering::Relaxed);
        }
        r.map(|(n, _)| n)
    }

    /// `.1`: a copy was left out (refused, failed to attach, or failed in the view swap).
    async fn refresh(&self, now: i64) -> Result<(usize, bool)> {
        let mut st = self.fleet.lock().await;
        let own = self.cfg.node.id.as_str();
        let grace = self.cfg.coordinator.snapshot_grace.ms();
        let mut left_out = false;
        let mut found: Vec<Manifest> = vec![];
        for (key, modified) in self.shared.list_modified("_fleet/").await? {
            let Some(name) = key.strip_prefix("_fleet/").and_then(|k| k.strip_suffix(".json")) else { continue };
            if name.contains('/') || name == own {
                continue;
            }
            // Not written for longer than the grace: a stopped node, too old to read anyway.
            if now - modified > grace {
                continue;
            }
            let m = match self.read_manifest(&key).await {
                Ok(m) => m,
                Err(e) => {
                    let e = format!("{e:#}");
                    if st.warned.insert((key.clone(), e.clone())) {
                        tracing::warn!(key, error = %e, "skipping a fleet manifest");
                    }
                    continue;
                }
            };
            if let Err(e) = check_manifest(name, &m) {
                if st.rejected.insert((key.clone(), m.epoch, m.snapshot_id)) {
                    left_out = true;
                    tracing::error!(key, error = %format!("{e:#}"), "rejecting a fleet manifest");
                }
                continue;
            }
            if now - m.published_at_ms > self.stale_after(&m) {
                if st.warned.insert((m.node_id.clone(), format!("stale {}", m.published_at_ms))) {
                    tracing::warn!(
                        node = %m.node_id, age_s = (now - m.published_at_ms) / 1000,
                        "skipping a fleet catalog copy too old to read safely: files it names may be deleted (see coordinator.snapshot_grace)"
                    );
                }
                continue;
            }
            found.push(m);
        }
        found.sort_by(|a, b| a.node_id.cmp(&b.node_id));

        let mut next: Vec<Attached> = vec![];
        // Copies attached by this refresh and the earliest sample time each makes visible.
        let mut earliest: Vec<(String, i64)> = vec![];
        for m in found {
            let prev = st.attached.iter().find(|c| c.m.node_id == m.node_id).cloned();
            if let Some(c) = &prev
                && c.m.epoch == m.epoch
                && c.m.snapshot_id == m.snapshot_id
                && c.m.catalog_key == m.catalog_key
            {
                // Same process, same snapshot: the attached copy is current as of this manifest.
                next.push(Attached { m, ..c.clone() });
                continue;
            }
            let fresh_prev = prev.clone().filter(|c| now - c.m.published_at_ms <= self.stale_after(&c.m));
            let rkey = (manifest_key(&m.node_id), m.epoch, m.snapshot_id);
            if st.rejected.contains(&rkey) {
                next.extend(fresh_prev);
                continue;
            }
            // New rows are those of the files added since the copy read before, while every file
            // added since is still listed (the publisher keeps them for its grace); else all rows.
            let since = prev
                .filter(|c| c.m.epoch == m.epoch && now - c.m.published_at_ms < c.m.snapshot_grace_ms.min(grace))
                .map(|c| c.m.snapshot_id);
            st.next_alias += 1;
            let alias = format!("fleet_{}", st.next_alias);
            let path = self.fleet_dir().join(format!("{alias}.ducklake"));
            match self.attach_copy(&m, &alias, &path, since).await {
                Ok(t) => {
                    earliest.extend(t.map(|t| (alias.clone(), t)));
                    next.push(Attached { m, alias, path });
                }
                Err(e) => {
                    left_out = true;
                    // Detached (if it got that far) and deleted below.
                    st.retired.push((alias, path, Instant::now()));
                    if e.downcast_ref::<Rejected>().is_some() {
                        st.rejected.insert(rkey);
                        tracing::error!(node = %m.node_id, snapshot = m.snapshot_id, error = %format!("{e:#}"), "rejecting another node's catalog copy");
                    } else {
                        tracing::warn!(node = %m.node_id, error = %format!("{e:#}"), "attaching a fleet catalog copy failed; keeping the previous one");
                    }
                    next.extend(fresh_prev);
                }
            }
        }

        let aliases = |v: &[Attached]| v.iter().map(|c| c.alias.clone()).collect::<Vec<_>>();
        let old = aliases(&st.attached);
        if old != aliases(&next) {
            let mut kept = next.clone();
            let mut res = self.swap_views(aliases(&kept)).await;
            if let Err(e) = &res {
                // One broken copy must not keep the views on stale or dropped ones: rebuild them
                // from the copies that still answer, else from this node's own data alone.
                left_out = true;
                tracing::warn!(error = %format!("{e:#}"), "swapping the query views failed; rebuilding them from the copies that answer");
                let answering = self.answering(aliases(&kept)).await;
                kept.retain(|c| answering.contains(&c.alias));
                res = self.swap_views(aliases(&kept)).await;
                if res.is_err() && !kept.is_empty() {
                    kept.clear();
                    res = self.swap_views(vec![]).await;
                }
            }
            let (now_i, after_queries) = (Instant::now(), Instant::now() + self.cfg.query.timeout.0);
            if let Err(e) = res {
                // The views still name the previous copies: keep those, give up the new ones.
                st.retired.extend(next.into_iter().filter(|c| !old.contains(&c.alias)).map(|c| (c.alias, c.path, now_i)));
                return Err(e.context("swapping the query views to the fleet's catalogs"));
            }
            let in_views = aliases(&kept);
            // Copies queries may still be reading go once no query from before the swap can run;
            // copies attached by this refresh and left out were never read.
            let gone: Vec<Attached> = st.attached.drain(..).filter(|c| !in_views.contains(&c.alias)).collect();
            st.retired.extend(gone.into_iter().map(|c| (c.alias, c.path, after_queries)));
            st.retired.extend(
                next.into_iter().filter(|c| !in_views.contains(&c.alias) && !old.contains(&c.alias)).map(|c| (c.alias, c.path, now_i)),
            );
            next = kept;
        }
        st.attached = next;

        let now_i = Instant::now();
        let (due, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut st.retired).into_iter().partition(|r| r.2 <= now_i);
        st.retired = keep;
        if !due.is_empty() {
            let conn = self.lake_conn()?;
            let failed = tokio::task::spawn_blocking(move || {
                due.into_iter()
                    .filter(|(alias, path, _)| match conn.execute_batch(&format!("DETACH DATABASE IF EXISTS {alias}")) {
                        Ok(()) => {
                            let _ = std::fs::remove_file(path);
                            let _ = std::fs::remove_file(format!("{}.wal", path.display()));
                            false
                        }
                        Err(e) => {
                            tracing::warn!(alias, error = %e, "detaching a replaced fleet catalog failed; will retry");
                            true
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .await?;
            // Still attached: kept (and its file) for the next refresh to try again.
            st.retired.extend(failed);
        }

        // Only once the rows are visible, against a fresh clock (as for a late local sample).
        let settled = now_ms() - self.cfg.materialize.settle.ms();
        for (alias, t) in earliest {
            if t <= settled && st.attached.iter().any(|c| c.alias == alias) {
                self.mat.note_late(t);
            }
        }
        let oldest = st.attached.iter().map(|c| c.m.published_at_ms).min().unwrap_or(0);
        self.metrics.fleet_nodes.store(st.attached.len() as u64, Ordering::Relaxed);
        self.metrics.fleet_oldest_published_ms.store(oldest.max(0) as u64, Ordering::Relaxed);
        Ok((st.attached.len(), left_out))
    }

    async fn read_manifest(&self, key: &str) -> Result<Manifest> {
        let b = self.shared.get(key).await?;
        let head: Head = serde_json::from_slice(&b).context("parsing")?;
        if head.format != FORMAT {
            bail!("manifest format {} (this build reads format {FORMAT})", head.format);
        }
        serde_json::from_slice(&b).context("parsing")
    }

    async fn swap_views(&self, aliases: Vec<String>) -> Result<()> {
        let conn = self.lake_conn()?;
        let (id, epoch) = (self.cfg.node.id.clone(), self.epoch);
        tokio::task::spawn_blocking(move || Node::create_views(&conn, &id, epoch, &aliases)).await?
    }

    /// The aliases whose every table can be read.
    async fn answering(&self, aliases: Vec<String>) -> HashSet<String> {
        let Ok(conn) = self.lake_conn() else { return HashSet::new() };
        tokio::task::spawn_blocking(move || aliases.into_iter().filter(|a| probe(&conn, a).is_ok()).collect()).await.unwrap_or_default()
    }

    /// Download a node's catalog copy, unpack it to `path`, check it and attach it read-only as
    /// `alias`. Returns the earliest sample time of the rows it shows that a copy attached with
    /// snapshot `since` did not (all rows when `None`).
    async fn attach_copy(&self, m: &Manifest, alias: &str, path: &Path, since: Option<i64>) -> Result<Option<i64>> {
        let max = self.cfg.fleet.max_copy_bytes.0;
        let gz = self.shared.get_at_most(&m.catalog_key, max).await?.ok_or_else(|| too_big(max))?;
        let conn = self.lake_conn()?;
        let prefix = format!("{}/{}/{}/", self.shared.url_prefix, m.region, m.node_id);
        let (alias, path) = (alias.to_string(), path.to_path_buf());
        tokio::task::spawn_blocking(move || -> Result<Option<i64>> {
            let mut out = std::fs::File::create(&path).with_context(|| format!("writing {}", path.display()))?;
            let n =
                std::io::copy(&mut flate2::read::GzDecoder::new(&gz[..]).take(max + 1), &mut out).context("unpacking the catalog copy")?;
            drop(out);
            if n > max {
                return Err(too_big(max));
            }
            // DuckLake reads the metadata database by table name: a view or generated column there
            // would run whatever it says. Look at the file as a plain database first.
            let raw = format!("{alias}_raw");
            conn.execute_batch(&format!("ATTACH {} AS {raw} (TYPE duckdb, READ_ONLY)", sql_str(&path.display().to_string())))
                .context("opening the catalog copy")?;
            let checked = check_metadata(&conn, &raw);
            conn.execute_batch(&format!("DETACH {raw}"))?;
            checked?;
            conn.execute_batch(&format!("ATTACH {} AS {alias} (READ_ONLY)", sql_str(&format!("ducklake:{}", path.display()))))
                .context("attaching the catalog copy")?;
            check_catalog(&conn, &alias, &prefix)?;
            probe(&conn, &alias).map_err(|e| rejected(format!("{e:#}")))?;
            earliest_sample(&conn, &alias, since)
        })
        .await?
    }

    /// Age in seconds of the oldest other-node catalog copy queries read (0 when alone).
    pub fn fleet_freshness_s(&self, now: i64) -> f64 {
        match self.metrics.fleet_oldest_published_ms.load(Ordering::Relaxed) as i64 {
            0 => 0.0,
            t => (now - t).max(0) as f64 / 1000.0,
        }
    }
}

fn too_big(max: u64) -> anyhow::Error {
    rejected(format!("the catalog copy is larger than fleet.max_copy_bytes ({max} bytes)"))
}

/// A manifest must name itself: `_fleet/<node_id>.json`, valid ids, its copy under its own prefix.
fn check_manifest(name: &str, m: &Manifest) -> Result<()> {
    check_id("node_id", &m.node_id)?;
    check_id("region", &m.region)?;
    if m.node_id != name {
        bail!("it names node {:?} but is _fleet/{name}.json", m.node_id);
    }
    let want = catalog_key(&m.region, &m.node_id);
    if m.catalog_key != want {
        bail!("its catalog_key {:?} is not {want:?}", m.catalog_key);
    }
    if m.snapshot_grace_ms <= 0 {
        bail!("its snapshot_grace_ms {} is not positive", m.snapshot_grace_ms);
    }
    Ok(())
}

/// The copy's metadata database (`db`, attached as a plain DuckDB file): nothing DuckDB would run
/// when DuckLake reads it, and no DuckLake views or macros (a node's catalog has none).
fn check_metadata(conn: &Connection, db: &str) -> Result<()> {
    let (views, macros, generated): (i64, i64, i64) = conn.query_row(
        "SELECT (SELECT count(*) FROM duckdb_views() WHERE database_name = $1 AND NOT internal),
                (SELECT count(*) FROM duckdb_functions() WHERE database_name = $1 AND function_type IN ('macro', 'table_macro')),
                (SELECT count(*) FROM duckdb_columns() WHERE database_name = $1 AND is_generated)",
        [db],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    if views + macros + generated > 0 {
        return Err(rejected(format!(
            "its metadata database holds {views} views, {macros} macros and {generated} generated columns (DuckLake writes none)"
        )));
    }
    let tables: Vec<String> = conn
        .prepare(
            "SELECT table_name FROM duckdb_tables() WHERE database_name = ? AND schema_name = 'main'
             AND table_name IN ('ducklake_view', 'ducklake_macro')",
        )?
        .query_map([db], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    for t in tables {
        // `t` is one of the two names above.
        let n: i64 = conn.query_row(&format!("SELECT count(*) FROM {db}.main.{t}"), [], |r| r.get(0))?;
        if n > 0 {
            return Err(rejected(format!("its catalog defines {n} {} (a node's catalog has none)", t.replace("ducklake_", "DuckLake "))));
        }
    }
    Ok(())
}

/// The attached copy `alias`: no views or macros, exactly this node's tables and columns, and
/// every data and delete file under `prefix` (the publisher's own `<storage.url>/<region>/<id>/`).
fn check_catalog(conn: &Connection, alias: &str, prefix: &str) -> Result<()> {
    let (views, macros): (i64, i64) = conn.query_row(
        "SELECT (SELECT count(*) FROM duckdb_views() WHERE database_name = $1),
                (SELECT count(*) FROM duckdb_functions() WHERE database_name = $1 AND function_type IN ('macro', 'table_macro'))",
        [alias],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if views + macros > 0 {
        return Err(rejected(format!("its catalog defines {views} views and {macros} macros (a node's catalog has none)")));
    }
    let columns = |db: &str| -> Result<BTreeSet<(String, String, String, String)>> {
        let mut s = conn.prepare("SELECT schema_name, table_name, column_name, data_type FROM duckdb_columns() WHERE database_name = ?")?;
        Ok(s.query_map([db], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<Result<_, _>>()?)
    };
    let (theirs, ours) = (columns(alias)?, columns("lake")?);
    if theirs != ours {
        let diff: Vec<String> = theirs.symmetric_difference(&ours).take(3).map(|(s, t, c, ty)| format!("{s}.{t}.{c} {ty}")).collect();
        return Err(rejected(format!("its tables and columns differ from this node's (e.g. {})", diff.join(", "))));
    }
    let tables: BTreeSet<&str> = ours.iter().map(|(_, t, _, _)| t.as_str()).collect();
    for t in tables {
        let mut s = conn.prepare(&format!("SELECT data_file, delete_file FROM ducklake_list_files({}, {})", sql_str(alias), sql_str(t)))?;
        let files: Vec<(String, Option<String>)> = s.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
        for f in files.iter().flat_map(|(d, del)| std::iter::once(d).chain(del)) {
            if !f.starts_with(prefix) || f.split('/').any(|seg| seg == "..") {
                return Err(rejected(format!("its table {t} names the file {f}, outside the publisher's prefix {prefix}")));
            }
        }
    }
    Ok(())
}

/// Every table the views read can be bound with the columns they read.
fn probe(conn: &Connection, alias: &str) -> Result<()> {
    for t in Table::ALL {
        conn.execute_batch(&format!("SELECT {} FROM {alias}.{} LIMIT 0", t.column_names().join(", "), t.name()))
            .with_context(|| format!("reading {alias}.{}", t.name()))?;
    }
    Ok(())
}

/// Earliest `timestamp_ms` of the metric samples in the copy `alias` from its file statistics:
/// of the L0 files (fresh flushes) added after snapshot `since`, else of every current file. A
/// file without statistics counts as the oldest possible. Compaction (L1) only rewrites rows of
/// files already counted, so it is left out.
fn earliest_sample(conn: &Connection, alias: &str, since: Option<i64>) -> Result<Option<i64>> {
    let md = format!("__ducklake_metadata_{alias}");
    let files = match since {
        Some(_) => "f.begin_snapshot > ? AND regexp_matches(f.path, '(^|/)l0-[^/]*$')",
        None => "f.end_snapshot IS NULL",
    };
    let sql = format!(
        "SELECT min(coalesce(TRY_CAST(s.min_value AS BIGINT), {min})) FROM {md}.ducklake_data_file f
         JOIN {md}.ducklake_table t ON t.table_id = f.table_id AND t.table_name = 'metrics_points' AND t.end_snapshot IS NULL
         LEFT JOIN {md}.ducklake_column c ON c.table_id = f.table_id AND c.column_name = 'timestamp_ms' AND c.end_snapshot IS NULL
         LEFT JOIN {md}.ducklake_file_column_stats s ON s.data_file_id = f.data_file_id AND s.column_id = c.column_id
         WHERE {files}",
        min = i64::MIN
    );
    let mut stmt = conn.prepare(&sql)?;
    let t: Option<i64> = match since {
        Some(s) => stmt.query_row([s], |r| r.get(0))?,
        None => stmt.query_row([], |r| r.get(0))?,
    };
    Ok(t)
}
