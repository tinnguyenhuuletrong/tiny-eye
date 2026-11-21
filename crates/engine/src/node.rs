//! One node in `--role=all` mode (SPEC §3): ingest, coordinator and query in one process.
//!
//! Write path (SPEC §5.1):
//!   rows → Arrow batches → WAL (fsync, group commit) → in-memory DuckDB "hot" table tagged
//!   with the open flush sequence → ack. Every `flush_interval` / `flush_bytes` the open
//!   sequence is sealed, exported per (table, hour) sorted by DuckDB, written by our Parquet
//!   writer (bloom, encodings), PUT, and one envelope is committed with If-None-Match.
//! Coordinator (SPEC §6): every `checkpoint_interval` pending envelopes are applied to DuckLake
//!   in one transaction (idempotent through `applied_envelopes`); hourly compaction swaps
//!   L0 → L1 atomically; retention drops expired files and DuckLake deletes the objects.
//! Read path (SPEC §7): queries read views that union the lake with hot rows whose sequence is
//!   above this node's committed watermark, so a row is counted exactly once (E7).

use crate::config::{Config, Durability};
use crate::metrics::Metrics;
use crate::store::{self, Store};
use crate::tables::*;
use crate::wal::{self, Wal};
use crate::{duck, parquet};
use anyhow::{Context, Result, anyhow, bail};
use arrow::array::{Int64Array, RecordBatch};
use arrow::datatypes::{DataType, Field, Schema};
use duckdb::Connection;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{RwLock, Semaphore};

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Rows from one ingest request. Receivers fill these; tenant/region are set by the node.
#[derive(Default, Debug)]
pub struct Rows {
    pub series: Vec<MetricsSeriesRow>,
    pub points: Vec<MetricsPointsRow>,
    pub spans: Vec<SpansRow>,
    pub logs: Vec<LogsRow>,
}

impl Rows {
    pub fn is_empty(&self) -> bool {
        self.series.is_empty() && self.points.is_empty() && self.spans.is_empty() && self.logs.is_empty()
    }
}

#[derive(Debug)]
pub enum IngestError {
    /// Arena above `ingest.arena_limit`: HTTP 429 / gRPC RESOURCE_EXHAUSTED.
    Overloaded,
    Internal(anyhow::Error),
}

impl std::fmt::Display for IngestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IngestError::Overloaded => write!(f, "ingest arena full; retry later"),
            IngestError::Internal(e) => write!(f, "{e:#}"),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FileEntry {
    pub table: String,
    pub path: String,
    pub bytes: u64,
    pub rows: u64,
}

/// SPEC §5.3: the only thing that makes a flush's files visible once applied.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Envelope {
    pub format: u32,
    pub envelope_id: String,
    pub kind: String,
    pub tenant: String,
    pub region: String,
    pub node_id: String,
    pub node_epoch: i64,
    pub seq_to: i64,
    pub files: Vec<FileEntry>,
}

struct OpenBatch {
    bytes: u64,
    rows: u64,
    opened: Instant,
    series_seen: HashSet<u64>,
}

/// Hot rows per sealed sequence: (bytes, committed_at once the catalog has them).
type Sealed = BTreeMap<i64, (u64, Option<Instant>)>;

pub struct Node {
    pub cfg: Config,
    pub store: Store,
    pub metrics: Metrics,
    pub epoch: i64,
    /// Root connection: owns the in-memory database (hot tables, views) and the lake attach.
    root: Mutex<Connection>,
    /// Appends to hot tables.
    writer: Arc<Mutex<Connection>>,
    /// The open flush sequence. Ingest holds it shared; sealing takes it exclusively so no
    /// append can land in a sequence after it was exported.
    seq: RwLock<i64>,
    open: Mutex<OpenBatch>,
    sealed: Mutex<Sealed>,
    hot_bytes: std::sync::atomic::AtomicU64,
    wal: Option<Wal>,
    pool: Arc<Mutex<Vec<Connection>>>,
    pool_gate: Arc<Semaphore>,
    /// Sealed sequences not yet written to object storage, oldest first. A newer sequence is
    /// never written while one of these is pending: the watermark is `max(seq_to)`, so a later
    /// commit would hide (and release) the rows of an earlier, unwritten sequence.
    unflushed: Mutex<std::collections::BTreeSet<i64>>,
    /// WAL segments of earlier processes whose rows were replayed into a sequence of this one;
    /// deleted once that sequence is in object storage (replay never copies, so a crash during
    /// replay cannot duplicate rows).
    replay_sources: Mutex<BTreeMap<i64, Vec<std::path::PathBuf>>>,
    /// Serialises flushes (the flusher task and `flush_now` from tests/benchmarks).
    flush_lock: tokio::sync::Mutex<()>,
    /// Serialises catalog writes (single coordinator: checkpoint, compaction, retention).
    catalog_lock: tokio::sync::Mutex<()>,
}

const APPLIED_DDL: &str = "CREATE TABLE IF NOT EXISTS lake.applied_envelopes (envelope_id VARCHAR, region VARCHAR, node_id VARCHAR, node_epoch BIGINT, seq_to BIGINT, files INTEGER)";

fn with_seq(batch: &RecordBatch, seq: i64) -> Result<RecordBatch> {
    let mut fields: Vec<_> = batch.schema().fields().iter().cloned().collect();
    fields.push(Arc::new(Field::new("_seq", DataType::Int64, false)));
    let mut cols = batch.columns().to_vec();
    cols.push(Arc::new(Int64Array::from(vec![seq; batch.num_rows()])));
    Ok(RecordBatch::try_new(Arc::new(Schema::new(fields)), cols)?)
}

/// A pooled query connection on loan; returned to the pool on drop (then the permit is freed).
struct Lease {
    conn: Option<Connection>,
    pool: Arc<Mutex<Vec<Connection>>>,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(c) = self.conn.take() {
            // A panic or interrupt can leave a transaction open; the next user must start clean.
            let _ = c.execute_batch("ROLLBACK");
            if let Ok(mut p) = self.pool.lock() {
                p.push(c);
            }
        }
    }
}

fn sql_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

impl Node {
    /// Open storage and catalog, create tables and views, replay the WAL.
    pub async fn open(cfg: Config) -> Result<Arc<Node>> {
        let store = Store::from_config(&cfg)?;
        let epoch = now_ms();
        let settings = duck::DuckSettings {
            extension_dir: cfg.duckdb.extension_dir.clone(),
            threads: cfg.duckdb.threads,
            memory_limit: cfg.duckdb.memory_limit.clone(),
        };
        let root = duck::open(&settings)?;
        if cfg.is_s3() {
            let s3 = &cfg.storage.s3;
            duck::add_s3_secret(
                &root,
                &duck::S3Secret {
                    endpoint: s3.endpoint.clone(),
                    region: s3.region.clone(),
                    key_id: s3.access_key_id.clone(),
                    secret: s3.secret_access_key.clone(),
                    use_ssl: !s3.endpoint.starts_with("http://"),
                },
            )?;
        }
        if !cfg.catalog.url.starts_with("postgres:")
            && let Some(parent) = std::path::Path::new(&cfg.catalog.url).parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        duck::attach_lake(&root, &cfg.catalog.url, &format!("{}/lake/", store.url_prefix))?;
        root.execute_batch(APPLIED_DDL)?;
        for t in Table::ALL {
            root.execute_batch(&t.ddl(&format!("lake.{}", t.name())))?;
            root.execute_batch(&format!("CREATE TABLE memory.main.hot_{} ({}, _seq BIGINT)", t.name(), t.ddl_columns()))?;
        }
        Self::create_views(&root, &cfg, epoch)?;
        let writer = Arc::new(Mutex::new(root.try_clone()?));
        let mut pool = vec![];
        for _ in 0..cfg.query.workers {
            pool.push(root.try_clone()?);
        }
        let wal = match cfg.ingest.durability {
            Durability::Wal => Some(Wal::open(std::path::Path::new(&cfg.ingest.wal_dir), epoch)?),
            Durability::Memory => None,
        };
        let node = Arc::new(Node {
            pool_gate: Arc::new(Semaphore::new(cfg.query.workers)),
            unflushed: Default::default(),
            replay_sources: Default::default(),
            cfg,
            store,
            metrics: Metrics::default(),
            epoch,
            root: Mutex::new(root),
            writer,
            seq: RwLock::new(1),
            open: Mutex::new(OpenBatch { bytes: 0, rows: 0, opened: Instant::now(), series_seen: HashSet::new() }),
            sealed: Mutex::new(BTreeMap::new()),
            hot_bytes: Default::default(),
            wal,
            pool: Arc::new(Mutex::new(pool)),
            flush_lock: Default::default(),
            catalog_lock: Default::default(),
        });
        node.replay_wal().await?;
        tracing::info!(epoch, node = %node.cfg.node.id, storage = %node.store.url_prefix, "node open");
        Ok(node)
    }

    /// Views the TQS compiler reads: lake ∪ hot rows above the committed watermark.
    fn create_views(conn: &Connection, cfg: &Config, epoch: i64) -> Result<()> {
        let wm = format!(
            "(SELECT coalesce(max(seq_to), 0) FROM lake.applied_envelopes WHERE node_id = {} AND node_epoch = {epoch})",
            sql_str(&cfg.node.id)
        );
        for t in Table::ALL {
            let cols = t.column_names().join(", ");
            let n = t.name();
            let union = format!("SELECT {cols} FROM lake.{n} UNION ALL SELECT {cols} FROM memory.main.hot_{n} WHERE _seq > {wm}");
            let body = match t {
                // One row per (series, flush) is stored; queries see one per series.
                Table::MetricsSeries => format!(
                    "SELECT series_id, any_value(metric_name) AS metric_name, any_value(metric_type) AS metric_type, any_value(labels) AS labels FROM ({union}) GROUP BY series_id"
                ),
                _ => union,
            };
            conn.execute_batch(&format!("CREATE OR REPLACE VIEW memory.main.{n} AS {body}"))?;
        }
        conn.execute_batch("USE memory.main")?;
        Ok(())
    }

    fn lake_conn(&self) -> Result<Connection> {
        let c = self.root.lock().map_err(|_| anyhow!("root lock poisoned"))?.try_clone()?;
        c.execute_batch("USE memory.main")?;
        Ok(c)
    }

    // ------------------------------------------------------------------ ingest

    /// Admit, persist per the durability mode, then return (= acknowledge).
    pub async fn ingest(&self, mut rows: Rows) -> Result<(), IngestError> {
        if rows.is_empty() {
            return Ok(());
        }
        let (tenant, region) = (&self.cfg.node.tenant, &self.cfg.node.region);
        let now = now_ms();
        let (lo, hi) = (now - self.cfg.ingest.max_sample_age.ms(), now + self.cfg.ingest.max_sample_future.ms());
        let before = rows.points.len() + rows.spans.len() + rows.logs.len();
        rows.points.retain(|p| p.timestamp_ms >= lo && p.timestamp_ms <= hi);
        rows.spans.retain(|s| s.start_time_ns / 1_000_000 >= lo && s.start_time_ns / 1_000_000 <= hi);
        rows.logs.retain(|l| l.timestamp_ns / 1_000_000 >= lo && l.timestamp_ns / 1_000_000 <= hi);
        let dropped = before - (rows.points.len() + rows.spans.len() + rows.logs.len());
        if dropped > 0 {
            self.metrics.rejected_out_of_bounds.fetch_add(dropped as u64, Ordering::Relaxed);
        }

        let seq_guard = self.seq.read().await;
        let seq = *seq_guard;
        {
            let open = self.open.lock().map_err(|_| IngestError::Internal(anyhow!("open lock poisoned")))?;
            // Only filter here; the ids are marked seen after the append succeeds, so a refused or
            // failed request does not hide its series rows from the retry.
            let mut in_request = HashSet::new();
            rows.series.retain(|s| !open.series_seen.contains(&s.series_id) && in_request.insert(s.series_id));
        }
        let mut batches: Vec<(Table, RecordBatch)> = vec![];
        macro_rules! build {
            ($vec:expr, $builder:ty, $table:expr) => {
                if !$vec.is_empty() {
                    let mut b = <$builder>::default();
                    for r in $vec.iter_mut() {
                        r.tenant.clone_from(tenant);
                        r.region.clone_from(region);
                        b.append(r);
                    }
                    batches.push(($table, b.finish()));
                }
            };
        }
        build!(rows.series, MetricsSeriesBuilder, Table::MetricsSeries);
        build!(rows.points, MetricsPointsBuilder, Table::MetricsPoints);
        build!(rows.spans, SpansBuilder, Table::Spans);
        build!(rows.logs, LogsBuilder, Table::Logs);
        if batches.is_empty() {
            return Ok(());
        }
        let bytes: u64 = batches.iter().map(|(_, b)| b.get_array_memory_size() as u64).sum();
        let nrows: u64 = batches.iter().map(|(_, b)| b.num_rows() as u64).sum();
        if self.hot_bytes.load(Ordering::Relaxed) + bytes > self.cfg.ingest.arena_limit.0 {
            self.metrics.rejected_overload.fetch_add(nrows, Ordering::Relaxed);
            return Err(IngestError::Overloaded);
        }
        if let Some(w) = &self.wal {
            let mut frames = Vec::with_capacity(bytes as usize / 2);
            for (t, b) in &batches {
                wal::encode(*t, b, &mut frames).map_err(IngestError::Internal)?;
            }
            w.append(seq, frames).await.map_err(IngestError::Internal)?;
        }
        let writer = self.writer.clone();
        let counts: Vec<(Table, u64)> = batches.iter().map(|(t, b)| (*t, b.num_rows() as u64)).collect();
        let new_series: Vec<u64> = rows.series.iter().map(|s| s.series_id).collect();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = writer.lock().map_err(|_| anyhow!("writer lock poisoned"))?;
            for (t, b) in batches {
                conn.appender_to_catalog_and_db(&format!("hot_{}", t.name()), "memory", "main")?.append_record_batch(with_seq(&b, seq)?)?;
            }
            Ok(())
        })
        .await
        .map_err(|e| IngestError::Internal(e.into()))?
        .map_err(IngestError::Internal)?;
        {
            let mut open = self.open.lock().map_err(|_| IngestError::Internal(anyhow!("open lock poisoned")))?;
            open.bytes += bytes;
            open.rows += nrows;
            open.series_seen.extend(new_series);
        }
        self.hot_bytes.fetch_add(bytes, Ordering::Relaxed);
        for (t, n) in counts {
            self.metrics.ingested(t, n);
        }
        drop(seq_guard);
        Ok(())
    }

    /// Rows of WAL segments that never reached object storage go back into the open batch.
    async fn replay_wal(&self) -> Result<()> {
        let dir = std::path::Path::new(&self.cfg.ingest.wal_dir);
        let leftovers = Wal::leftovers(dir)?;
        if leftovers.is_empty() {
            return Ok(());
        }
        // A segment whose envelope was already committed is skipped: (epoch, seq) recorded.
        let committed: HashSet<(i64, i64)> = {
            let conn = self.lake_conn()?;
            let mut stmt = conn.prepare("SELECT node_epoch, seq_to FROM lake.applied_envelopes WHERE node_id = ?")?;
            stmt.query_map([&self.cfg.node.id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?
        };
        let pending: Vec<Envelope> = self.pending_envelopes().await?.into_iter().map(|(_, e)| e).collect();
        let mut replayed = 0usize;
        for (epoch, seq, path) in leftovers {
            if epoch == self.epoch {
                continue;
            }
            let done = committed.contains(&(epoch, seq))
                || pending.iter().any(|e| e.node_id == self.cfg.node.id && e.node_epoch == epoch && e.seq_to == seq);
            if !done {
                let recs = Wal::read(&path).with_context(|| format!("replaying {}", path.display()))?;
                let seq_now = *self.seq.read().await;
                let conn = self.writer.lock().map_err(|_| anyhow!("writer lock poisoned"))?;
                let (mut bytes, mut rows) = (0u64, 0u64);
                for (t, b) in recs {
                    bytes += b.get_array_memory_size() as u64;
                    rows += b.num_rows() as u64;
                    replayed += b.num_rows();
                    conn.appender_to_catalog_and_db(&format!("hot_{}", t.name()), "memory", "main")?
                        .append_record_batch(with_seq(&b, seq_now)?)?;
                }
                let mut open = self.open.lock().map_err(|_| anyhow!("open lock poisoned"))?;
                open.bytes += bytes;
                open.rows += rows;
                self.hot_bytes.fetch_add(bytes, Ordering::Relaxed);
                drop(open);
                drop(conn);
                // Keep the old segment until `seq_now` is in object storage (see replay_sources).
                self.replay_sources.lock().map_err(|_| anyhow!("replay lock poisoned"))?.entry(seq_now).or_default().push(path);
            } else {
                std::fs::remove_file(&path)?;
            }
        }
        if replayed > 0 {
            tracing::warn!(rows = replayed, "replayed unflushed rows from the WAL");
        }
        Ok(())
    }

    // ------------------------------------------------------------------ flush

    pub fn flush_due(&self) -> bool {
        let Ok(open) = self.open.lock() else { return false };
        open.rows > 0 && (open.bytes >= self.cfg.ingest.flush_bytes.0 || open.opened.elapsed() >= self.cfg.ingest.flush_interval.0)
    }

    /// Seal the open sequence and write it to object storage with its envelope. Sequences that
    /// failed earlier are written first, in order; while one keeps failing nothing newer is
    /// sealed (ingest continues into the open batch until the arena limit answers 429).
    pub async fn flush_now(&self) -> Result<Option<Envelope>> {
        let _g = self.flush_lock.lock().await;
        let mut last = None;
        let retry: Vec<i64> = self.unflushed.lock().map_err(|_| anyhow!("unflushed lock poisoned"))?.iter().copied().collect();
        for seq in retry {
            last = Some(self.write_sequence(seq).await?);
        }
        let sealed = {
            let mut s = self.seq.write().await;
            let mut open = self.open.lock().map_err(|_| anyhow!("open lock poisoned"))?;
            if open.rows == 0 {
                None
            } else {
                let sealed = (*s, open.bytes);
                *s += 1;
                *open = OpenBatch { bytes: 0, rows: 0, opened: Instant::now(), series_seen: HashSet::new() };
                Some(sealed)
            }
        };
        if let Some((seq, bytes)) = sealed {
            self.sealed.lock().map_err(|_| anyhow!("sealed lock poisoned"))?.insert(seq, (bytes, None));
            self.unflushed.lock().map_err(|_| anyhow!("unflushed lock poisoned"))?.insert(seq);
            last = Some(self.write_sequence(seq).await?);
        }
        Ok(last)
    }

    /// The envelope key is deterministic per (node, epoch, sequence), so the create-only PUT
    /// turns a retry after a lost response into a no-op instead of a second commit.
    fn envelope_key(&self, seq: i64) -> String {
        format!("commits/{}/{}-{:013}-{seq:012}.json", self.cfg.node.region, self.cfg.node.id, self.epoch)
    }

    /// Export one sealed sequence per (table, hour), write it, and commit its envelope.
    async fn write_sequence(&self, seq: i64) -> Result<Envelope> {
        let started = Instant::now();
        let conn = self.lake_conn()?;
        let rgr = self.cfg.ingest.row_group_rows;
        let files: Vec<(Table, i64, bytes::Bytes, u64)> = tokio::task::spawn_blocking(move || -> Result<_> {
            let mut out = vec![];
            for t in Table::ALL {
                let n = t.name();
                let hours: Vec<i64> = conn
                    .prepare(&format!("SELECT DISTINCT {}::BIGINT FROM memory.main.hot_{n} WHERE _seq = {seq}", t.hour_sql()))?
                    .query_map([], |r| r.get(0))?
                    .collect::<Result<_, _>>()
                    .with_context(|| format!("listing hours of {n}"))?;
                for h in hours {
                    let mut stmt = conn.prepare(&format!(
                        "SELECT * EXCLUDE (_seq) FROM memory.main.hot_{n} WHERE _seq = {seq} AND {} = {h}{} ORDER BY {}",
                        t.hour_sql(),
                        t.dedup_sql(),
                        t.sort_columns().join(", ")
                    ))?;
                    let batches: Vec<RecordBatch> = stmt.query_arrow([]).with_context(|| format!("exporting {n} hour {h}"))?.collect();
                    let rows: u64 = batches.iter().map(|b| b.num_rows() as u64).sum();
                    out.push((t, h, parquet::write(t, &batches, rgr).with_context(|| format!("writing {n} hour {h}"))?, rows));
                }
            }
            Ok(out)
        })
        .await??;
        let mut entries = vec![];
        for (t, hour, data, rows) in files {
            let key = store::data_key(t.name(), &self.cfg.node.tenant, hour, "l0", &self.cfg.node.id);
            let len = data.len() as u64;
            self.store.put(&key, data).await?;
            self.metrics.flushed_bytes.fetch_add(len, Ordering::Relaxed);
            entries.push(FileEntry { table: t.name().into(), path: self.store.url(&key), bytes: len, rows });
        }
        let mut env = Envelope {
            format: 1,
            envelope_id: ulid::Ulid::new().to_string(),
            kind: "ingest".into(),
            tenant: self.cfg.node.tenant.clone(),
            region: self.cfg.node.region.clone(),
            node_id: self.cfg.node.id.clone(),
            node_epoch: self.epoch,
            seq_to: seq,
            files: entries,
        };
        let key = self.envelope_key(seq);
        if !self.store.put_new(&key, serde_json::to_vec(&env)?.into()).await? {
            // An earlier attempt committed this sequence; its envelope (and files) win.
            env = serde_json::from_slice(&self.store.get(&key).await?)?;
        }
        if let Some(w) = &self.wal {
            w.remove(seq);
        }
        for path in self.replay_sources.lock().map_err(|_| anyhow!("replay lock poisoned"))?.remove(&seq).unwrap_or_default() {
            let _ = std::fs::remove_file(path);
        }
        self.unflushed.lock().map_err(|_| anyhow!("unflushed lock poisoned"))?.remove(&seq);
        self.metrics.flushes.fetch_add(1, Ordering::Relaxed);
        self.metrics.flush_ms.fetch_add(started.elapsed().as_millis() as u64, Ordering::Relaxed);
        tracing::debug!(seq, files = env.files.len(), ms = started.elapsed().as_millis() as u64, "flushed");
        Ok(env)
    }

    // ------------------------------------------------------------------ coordinator

    async fn pending_envelopes(&self) -> Result<Vec<(String, Envelope)>> {
        let mut keys = self.store.list(&format!("commits/{}/", self.cfg.node.region)).await?;
        keys.sort();
        let mut out = vec![];
        for k in keys {
            let env: Envelope = serde_json::from_slice(&self.store.get(&k).await?).with_context(|| format!("parsing envelope {k}"))?;
            if env.format != 1 {
                bail!("envelope {k} has format {} but this binary reads format 1", env.format);
            }
            out.push((k, env));
        }
        Ok(out)
    }

    /// Apply every pending envelope in one catalog transaction (SPEC §6.1), then delete them.
    pub async fn checkpoint(&self) -> Result<usize> {
        let _g = self.catalog_lock.lock().await;
        let listed = self.pending_envelopes().await?;
        let pending: Vec<Envelope> = listed.iter().map(|(_, e)| e.clone()).collect();
        if pending.is_empty() {
            self.release_hot()?;
            return Ok(0);
        }
        let conn = self.lake_conn()?;
        let envs = pending.clone();
        let applied = tokio::task::spawn_blocking(move || -> Result<usize> {
            conn.execute_batch("BEGIN")?;
            let res = (|| -> Result<usize> {
                let mut n = 0;
                for env in &envs {
                    let seen: i64 =
                        conn.query_row("SELECT count(*) FROM lake.applied_envelopes WHERE envelope_id = ?", [&env.envelope_id], |r| {
                            r.get(0)
                        })?;
                    if seen > 0 {
                        continue; // crash between COMMIT and envelope delete: already applied
                    }
                    for f in &env.files {
                        if Table::from_name(&f.table).is_none() {
                            bail!("envelope {} names unknown table {}", env.envelope_id, f.table);
                        }
                        conn.execute(&format!("CALL ducklake_add_data_files('lake', {}, {})", sql_str(&f.table), sql_str(&f.path)), [])
                            .with_context(|| format!("registering {}", f.path))?;
                    }
                    conn.execute(
                        "INSERT INTO lake.applied_envelopes VALUES (?, ?, ?, ?, ?, ?)",
                        duckdb::params![env.envelope_id, env.region, env.node_id, env.node_epoch, env.seq_to, env.files.len() as i32],
                    )?;
                    n += 1;
                }
                Ok(n)
            })();
            match res {
                Ok(n) => {
                    conn.execute_batch("COMMIT")?;
                    Ok(n)
                }
                Err(e) => {
                    let _ = conn.execute_batch("ROLLBACK");
                    Err(e)
                }
            }
        })
        .await??;
        let now = Instant::now();
        {
            let mut sealed = self.sealed.lock().map_err(|_| anyhow!("sealed lock poisoned"))?;
            for env in pending.iter().filter(|e| e.node_id == self.cfg.node.id && e.node_epoch == self.epoch) {
                for (_, v) in sealed.range_mut(..=env.seq_to) {
                    v.1.get_or_insert(now);
                }
            }
        }
        for (key, _) in &listed {
            self.store.delete(key).await?;
        }
        self.metrics.checkpoints.fetch_add(1, Ordering::Relaxed);
        self.release_hot()?;
        Ok(applied)
    }

    /// Drop hot rows whose sequence the catalog has had for longer than `hot_grace`.
    fn release_hot(&self) -> Result<()> {
        let grace = self.cfg.coordinator.hot_grace.0;
        let upto = {
            let sealed = self.sealed.lock().map_err(|_| anyhow!("sealed lock poisoned"))?;
            sealed.iter().take_while(|(_, (_, c))| c.is_some_and(|t| t.elapsed() >= grace)).map(|(s, _)| *s).last()
        };
        let Some(upto) = upto else { return Ok(()) };
        let conn = self.writer.lock().map_err(|_| anyhow!("writer lock poisoned"))?;
        for t in Table::ALL {
            conn.execute(&format!("DELETE FROM memory.main.hot_{} WHERE _seq <= {upto}", t.name()), [])?;
        }
        let mut sealed = self.sealed.lock().map_err(|_| anyhow!("sealed lock poisoned"))?;
        let keep = sealed.split_off(&(upto + 1));
        let freed: u64 = sealed.values().map(|(b, _)| *b).sum();
        *sealed = keep;
        self.hot_bytes.fetch_sub(freed.min(self.hot_bytes.load(Ordering::Relaxed)), Ordering::Relaxed);
        Ok(())
    }

    /// Registered data files of a table: (url, key, hour, is_l0).
    fn lake_files(conn: &Connection, store: &Store, t: Table) -> Result<Vec<(String, i64, bool)>> {
        let mut stmt = conn.prepare(&format!("SELECT data_file FROM ducklake_list_files('lake', {})", sql_str(t.name())))?;
        let urls: Vec<String> = stmt.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
        Ok(urls
            .into_iter()
            .filter_map(|u| {
                let key = store.key_of(&u)?;
                let hour = store::hour_of_key(&key)?;
                let l0 = key.rsplit('/').next().is_some_and(|f| f.starts_with("l0-"));
                Some((u, hour, l0))
            })
            .collect())
    }

    /// Compact every closed hour that still has L0 files into one L1 file per table (SPEC §6.3).
    pub async fn compact(&self, now: i64) -> Result<usize> {
        let _g = self.catalog_lock.lock().await;
        let conn = self.lake_conn()?;
        let store = self.store.clone();
        let delay = self.cfg.coordinator.compaction_delay.ms();
        let rgr = self.cfg.ingest.row_group_rows;
        let jobs: Vec<(Table, i64, Vec<String>, bytes::Bytes)> = tokio::task::spawn_blocking(move || -> Result<_> {
            let mut out = vec![];
            for t in Table::ALL {
                let files = Self::lake_files(&conn, &store, t)?;
                let mut by_hour: BTreeMap<i64, (Vec<String>, bool)> = BTreeMap::new();
                for (u, h, l0) in files {
                    let e = by_hour.entry(h).or_default();
                    e.0.push(u);
                    e.1 |= l0;
                }
                for (h, (urls, has_l0)) in by_hour {
                    if !has_l0 || (h + 1) * 3_600_000 + delay > now {
                        continue;
                    }
                    let list = urls.iter().map(|u| sql_str(u)).collect::<Vec<_>>().join(", ");
                    // One row per identity (tables.yaml `unique`): retried requests collapse here.
                    let src = format!("(SELECT * FROM read_parquet([{list}], union_by_name = true){})", t.dedup_sql());
                    let sort = t.sort_columns().join(", ");
                    let cols = t.column_names().join(", ");
                    let sql = match t {
                        // positive_delta: the increase since the previous sample, reset-corrected (D2).
                        Table::MetricsPoints => format!(
                            "SELECT tenant, region, series_id, timestamp_ms, value, is_stale,
                                    CASE WHEN lag(value) OVER w IS NULL THEN NULL WHEN value >= lag(value) OVER w THEN value - lag(value) OVER w ELSE value END AS positive_delta
                             FROM {src} WINDOW w AS (PARTITION BY series_id ORDER BY timestamp_ms) ORDER BY {sort}"
                        ),
                        Table::MetricsSeries => format!("SELECT {cols} FROM {src} ORDER BY {sort}"),
                        _ => format!("SELECT {cols} FROM {src} ORDER BY {sort}"),
                    };
                    let batches: Vec<RecordBatch> = conn.prepare(&sql)?.query_arrow([])?.collect();
                    out.push((t, h, urls, parquet::write(t, &batches, rgr)?));
                }
            }
            Ok(out)
        })
        .await??;
        let n = jobs.len();
        for (t, hour, old, data) in jobs {
            let key = store::data_key(t.name(), &self.cfg.node.tenant, hour, "l1", "compacted");
            self.store.put(&key, data).await?;
            let url = self.store.url(&key);
            let conn = self.lake_conn()?;
            tokio::task::spawn_blocking(move || -> Result<()> {
                let list = old.iter().map(|u| sql_str(u)).collect::<Vec<_>>().join(", ");
                conn.execute_batch(&format!(
                    "BEGIN; DELETE FROM lake.{n} WHERE filename IN ({list}); CALL ducklake_add_data_files('lake', {tn}, {u}); COMMIT;",
                    n = t.name(),
                    tn = sql_str(t.name()),
                    u = sql_str(&url)
                ))
                .inspect_err(|_| {
                    let _ = conn.execute_batch("ROLLBACK");
                })?;
                Ok(())
            })
            .await??;
            self.metrics.compactions.fetch_add(1, Ordering::Relaxed);
        }
        Ok(n)
    }

    /// Drop files older than `retention`, expire old snapshots and let DuckLake delete the
    /// objects nothing references any more (SPEC §6.4).
    pub async fn retain(&self, now: i64) -> Result<usize> {
        let _g = self.catalog_lock.lock().await;
        let conn = self.lake_conn()?;
        let store = self.store.clone();
        let cutoff_hour = (now - self.cfg.coordinator.retention.ms()) / 3_600_000;
        tokio::task::spawn_blocking(move || -> Result<usize> {
            let mut dropped = 0;
            for t in Table::ALL {
                let old: Vec<String> =
                    Self::lake_files(&conn, &store, t)?.into_iter().filter(|(_, h, _)| *h < cutoff_hour).map(|(u, _, _)| u).collect();
                if !old.is_empty() {
                    let list = old.iter().map(|u| sql_str(u)).collect::<Vec<_>>().join(", ");
                    conn.execute_batch(&format!("DELETE FROM lake.{} WHERE filename IN ({list})", t.name()))?;
                    dropped += old.len();
                }
            }
            conn.execute_batch(&format!(
                "DELETE FROM lake.applied_envelopes WHERE envelope_id < {};
                 CALL ducklake_expire_snapshots('lake', older_than => to_timestamp({}));
                 CALL ducklake_cleanup_old_files('lake', cleanup_all => true);",
                // ULIDs sort by time: keep a day of envelope ids for crash replay checks.
                sql_str(&ulid::Ulid::from_parts((now - 86_400_000).max(0) as u64, 0).to_string()),
                // Snapshots older than 10 min: no query (30 s timeout) can still be reading them.
                (now - 600_000) / 1000
            ))?;
            Ok(dropped)
        })
        .await?
    }

    /// Background loops: flusher + coordinator. Returns when `stop` fires.
    pub async fn run_background(self: Arc<Self>, mut stop: tokio::sync::watch::Receiver<bool>) {
        let c = &self.cfg.coordinator;
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        let mut last_ckpt = Instant::now();
        let mut last_compact = Instant::now();
        let mut last_retain = Instant::now() - c.retention_interval.0;
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                _ = stop.changed() => break,
            }
            if self.flush_due()
                && let Err(e) = self.flush_now().await
            {
                tracing::error!(error = %format!("{e:#}"), "flush failed; will retry");
            }
            if last_ckpt.elapsed() >= c.checkpoint_interval.0 {
                last_ckpt = Instant::now();
                if let Err(e) = self.checkpoint().await {
                    tracing::error!(error = %format!("{e:#}"), "checkpoint failed; will retry");
                }
            }
            if last_compact.elapsed() >= c.compaction_interval.0 {
                last_compact = Instant::now();
                if let Err(e) = self.compact(now_ms()).await {
                    tracing::error!(error = %format!("{e:#}"), "compaction failed; will retry");
                }
            }
            if last_retain.elapsed() >= c.retention_interval.0 {
                last_retain = Instant::now();
                if let Err(e) = self.retain(now_ms()).await {
                    tracing::error!(error = %format!("{e:#}"), "retention failed; will retry");
                }
            }
        }
    }

    /// Flush what is buffered and apply it (clean shutdown; tests and benchmarks).
    pub async fn flush_and_checkpoint(&self) -> Result<()> {
        self.flush_now().await?;
        self.checkpoint().await?;
        Ok(())
    }

    // ------------------------------------------------------------------ query

    /// Run `f` on a pooled query connection on a blocking thread, with the query timeout.
    pub async fn with_conn<T: Send + 'static>(&self, f: impl FnOnce(&Connection) -> Result<T> + Send + 'static) -> Result<T> {
        let permit = self.pool_gate.clone().acquire_owned().await?;
        let conn = self.pool.lock().map_err(|_| anyhow!("pool lock poisoned"))?.pop().context("query pool empty")?;
        let interrupt = conn.interrupt_handle();
        let pool = self.pool.clone();
        // The blocking job owns the connection and the permit and gives both back when it ends,
        // however it ends: finished, interrupted by the timeout, panicked, or abandoned because
        // the HTTP client went away (which drops this future, not the blocking job).
        let job = tokio::task::spawn_blocking(move || {
            let lease = Lease { conn: Some(conn), pool, _permit: permit };
            f(lease.conn.as_ref().expect("leased connection"))
        });
        let timeout = self.cfg.query.timeout.0;
        match tokio::time::timeout(timeout, job).await {
            Ok(joined) => joined.map_err(|e| anyhow!("query task failed: {e}"))?,
            Err(_) => {
                interrupt.interrupt();
                bail!("query exceeded the {} ms timeout", timeout.as_millis());
            }
        }
    }

    pub fn hot_bytes(&self) -> u64 {
        self.hot_bytes.load(Ordering::Relaxed)
    }
}
