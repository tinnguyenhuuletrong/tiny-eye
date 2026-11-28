//! A fleet: nodes that share `storage.url` find each other there and answer queries over all
//! their data. No peers list, no coordinator, no shared catalog: every node is a full node with
//! its own catalog file and its own object prefix `<region>/<id>/`.
//!
//! Publish (every `fleet.publish_interval`, under the catalog lock so it never overlaps a
//! checkpoint): copy the DuckLake metadata into a fresh DuckDB file, gzip it to
//! `<region>/<id>/_fleet/catalog.ducklake.gz`, then write the manifest `_fleet/<id>.json`.
//! Refresh (every `fleet.refresh_interval`): list `_fleet/*.json`, skip this node and manifests
//! older than `coordinator.snapshot_grace`, download the copies that changed, attach them
//! read-only under new aliases and swap the query views to own lake ∪ own hot tail ∪ every other
//! node's tables. Replaced aliases are detached once a query that started before the swap can no
//! longer be running (`query.timeout`).
//! Safe deletion: retention expires a snapshot only once a newer one has existed for
//! `snapshot_grace` (see `Node::retain_lake`), so a copy at most publish + refresh + query
//! timeout old never names a deleted file.
//!
//! No label is added per node or region: like Prometheus external labels, senders label their
//! region. Series ids include the receiving node's region, so one label set sent to two regions
//! stays two series (aggregations add them up); sent to two nodes of one region it is one series.

use crate::node::{Node, sql_str};
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Instant;

/// Version of the manifest and catalog copy; a node skips manifests of another format.
pub const FORMAT: u32 = 1;
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
    /// Key of the gzipped catalog copy, relative to `storage.url`.
    pub catalog_key: String,
}

pub fn manifest_key(node_id: &str) -> String {
    format!("_fleet/{node_id}.json")
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
    /// Aliases no view names any more, with the time they were replaced.
    retired: Vec<(String, PathBuf, Instant)>,
    /// Stale manifests already warned about: (node id, published_at_ms).
    warned: HashSet<(String, i64)>,
}

impl Node {
    /// Local directory of this node's downloaded copies (`fleet.dir/<node.id>`).
    pub(crate) fn fleet_dir(&self) -> PathBuf {
        Path::new(&self.cfg.fleet.dir).join(&self.cfg.node.id)
    }

    /// Publish this node's catalog for the others: the copy (only when the snapshot changed since
    /// this process last published), then the manifest pointing at it.
    pub async fn publish(&self) -> Result<Manifest> {
        let _g = self.catalog_lock.lock().await;
        let id = &self.cfg.node.id;
        let mkey = manifest_key(id);
        let last = *self.published.lock().map_err(|_| anyhow!("publish lock poisoned"))?;
        // Only this process writes our manifest once it has: another epoch there means another
        // process publishes under the same node id (and shares our prefix in this region).
        if last.is_some()
            && let Ok(b) = self.shared.get(&mkey).await
            && let Ok(m) = serde_json::from_slice::<Manifest>(&b)
            && m.epoch != self.epoch
        {
            self.metrics.fleet_errors.fetch_add(1, Ordering::Relaxed);
            bail!(
                "node.id {id:?} is used by another node too: {mkey} was last written by the process started at {} (this one started \
                 at {}); every node sharing storage.url needs its own node.id",
                m.epoch,
                self.epoch
            );
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
            published_at_ms: crate::node::now_ms(),
            catalog_key: format!("{}/{CATALOG_KEY}", self.cfg.node.prefix()),
        };
        self.shared.put(&mkey, serde_json::to_vec(&m)?.into()).await?;
        *self.published.lock().map_err(|_| anyhow!("publish lock poisoned"))? = Some(snapshot_id);
        self.metrics.fleet_publishes.fetch_add(1, Ordering::Relaxed);
        Ok(m)
    }

    /// Attach the current catalog copies of the other nodes and swap the query views to them.
    /// `now` decides which manifests are stale. Returns how many other nodes queries now read.
    pub async fn refresh_fleet(&self, now: i64) -> Result<usize> {
        let mut st = self.fleet.lock().await;
        let grace = self.cfg.coordinator.snapshot_grace.ms();
        let mut found: Vec<Manifest> = vec![];
        for key in self.shared.list("_fleet/").await? {
            if !key.ends_with(".json") || key.matches('/').count() != 1 {
                continue;
            }
            let m = match self.shared.get(&key).await.and_then(|b| serde_json::from_slice::<Manifest>(&b).context("parsing")) {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(key, error = %format!("{e:#}"), "skipping an unreadable fleet manifest");
                    continue;
                }
            };
            if m.node_id == self.cfg.node.id {
                continue;
            }
            if m.format != FORMAT {
                tracing::warn!(key, format = m.format, "skipping a fleet manifest of another format (format {FORMAT} is read)");
                continue;
            }
            if now - m.published_at_ms > grace {
                if st.warned.insert((m.node_id.clone(), m.published_at_ms)) {
                    tracing::warn!(
                        node = %m.node_id, age_s = (now - m.published_at_ms) / 1000,
                        "skipping a fleet manifest older than coordinator.snapshot_grace: files its catalog names may be deleted"
                    );
                }
                continue;
            }
            found.push(m);
        }
        found.sort_by(|a, b| a.node_id.cmp(&b.node_id));

        let mut next: Vec<Attached> = vec![];
        for m in found {
            let prev = st.attached.iter().find(|c| c.m.node_id == m.node_id).cloned();
            if let Some(c) = &prev
                && c.m.snapshot_id == m.snapshot_id
                && c.m.catalog_key == m.catalog_key
            {
                // Same snapshot: the attached copy is current as of this manifest.
                next.push(Attached { m, ..c.clone() });
                continue;
            }
            st.next_alias += 1;
            let alias = format!("fleet_{}", st.next_alias);
            let path = self.fleet_dir().join(format!("{alias}.ducklake"));
            match self.attach_copy(&m, &alias, &path).await {
                Ok(()) => next.push(Attached { m, alias, path }),
                Err(e) => {
                    self.metrics.fleet_errors.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(node = %m.node_id, error = %format!("{e:#}"), "attaching a fleet catalog copy failed; keeping the previous one");
                    if let Some(c) = prev.filter(|c| now - c.m.published_at_ms <= grace) {
                        next.push(c);
                    }
                }
            }
        }

        let old: Vec<String> = st.attached.iter().map(|c| c.alias.clone()).collect();
        let new: Vec<String> = next.iter().map(|c| c.alias.clone()).collect();
        if old != new {
            let conn = self.lake_conn()?;
            let (id, epoch, aliases) = (self.cfg.node.id.clone(), self.epoch, new.clone());
            let swapped = tokio::task::spawn_blocking(move || Node::create_views(&conn, &id, epoch, &aliases)).await?;
            let replaced = Instant::now();
            if let Err(e) = swapped {
                // The views still name the old aliases: give up what was just attached.
                st.retired.extend(next.into_iter().filter(|c| !old.contains(&c.alias)).map(|c| (c.alias, c.path, replaced)));
                return Err(e.context("swapping the query views to the fleet's catalogs"));
            }
            let gone: Vec<Attached> = st.attached.drain(..).filter(|c| !new.contains(&c.alias)).collect();
            st.retired.extend(gone.into_iter().map(|c| (c.alias, c.path, replaced)));
        }
        st.attached = next;

        // A query that started before a swap may still read the replaced aliases until it times out.
        let timeout = self.cfg.query.timeout.0;
        let (due, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut st.retired).into_iter().partition(|r| r.2.elapsed() >= timeout);
        st.retired = keep;
        if !due.is_empty() {
            let conn = self.lake_conn()?;
            tokio::task::spawn_blocking(move || {
                for (alias, path, _) in due {
                    if let Err(e) = conn.execute_batch(&format!("DETACH DATABASE IF EXISTS {alias}")) {
                        tracing::warn!(alias, error = %e, "detaching a replaced fleet catalog failed");
                    }
                    let _ = std::fs::remove_file(path);
                }
            })
            .await?;
        }
        let oldest = st.attached.iter().map(|c| c.m.published_at_ms).min().unwrap_or(0);
        self.metrics.fleet_nodes.store(st.attached.len() as u64, Ordering::Relaxed);
        self.metrics.fleet_oldest_published_ms.store(oldest.max(0) as u64, Ordering::Relaxed);
        Ok(st.attached.len())
    }

    /// Download a node's catalog copy, unpack it to `path` and attach it read-only as `alias`.
    async fn attach_copy(&self, m: &Manifest, alias: &str, path: &Path) -> Result<()> {
        let gz = self.shared.get(&m.catalog_key).await?;
        let conn = self.lake_conn()?;
        let (alias, path) = (alias.to_string(), path.to_path_buf());
        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut bytes = vec![];
            flate2::read::GzDecoder::new(&gz[..]).read_to_end(&mut bytes).context("unpacking the catalog copy")?;
            std::fs::write(&path, &bytes).with_context(|| format!("writing {}", path.display()))?;
            let loc = sql_str(&format!("ducklake:{}", path.display()));
            if let Err(e) = conn.execute_batch(&format!("ATTACH {loc} AS {alias} (READ_ONLY)")) {
                let _ = std::fs::remove_file(&path);
                return Err(e).context("attaching the catalog copy");
            }
            Ok(())
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
