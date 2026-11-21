//! The experiments. Each one states the concept claim it tests and records a PASS/FAIL
//! with the numbers it measured. They run in order on one fresh stack (`poc all`).

use crate::synth::{self, NODES_PER_REGION, Span, WINDOWS};
use crate::lake::{self, Env, REGIONS};
use crate::write::{self, Envelope, FileEntry, Flusher, SpanLayout};
use anyhow::{Context, Result, anyhow, bail};
use arrow::array::RecordBatch;
use duckdb::Connection;
use duckdb::vtab::{ArrowBatchRegistration, arrow::ArrowVTab};
use futures::TryStreamExt;
use object_store::{ObjectStore, ObjectStoreExt, PutMode, PutOptions, PutPayload, UpdateVersion, path::Path};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

pub struct Finding {
    pub id: &'static str,
    pub claim: &'static str,
    pub pass: bool,
    pub evidence: Vec<String>,
}

pub struct Ctx {
    pub env: Env,
    pub stores: HashMap<&'static str, Arc<dyn ObjectStore>>,
    pub conn: Connection,
    /// Every span the generator produced, and the ones that reached object storage.
    pub written_spans: Vec<Span>,
    pub counters: synth::CounterSet,
    pub written_samples: Vec<synth::Sample>,
    pub probe_trace: [u8; 16],
    pub flushers: HashMap<(&'static str, usize), Flusher>,
    pub findings: Vec<Finding>,
}

fn hex(id: &[u8; 16]) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}

fn ms(t: Instant) -> f64 {
    (t.elapsed().as_secs_f64() * 1000.0 * 10.0).round() / 10.0
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn count(conn: &Connection, sql: &str) -> Result<i64> {
    conn.query_row(sql, [], |r| r.get(0)).with_context(|| sql.to_string())
}

impl Ctx {
    pub fn new(env: Env) -> Result<Self> {
        let mut stores: HashMap<&'static str, Arc<dyn ObjectStore>> = HashMap::new();
        for r in REGIONS {
            stores.insert(r, env.store(r)?);
        }
        let conn = env.lake()?;
        conn.execute_batch(lake::schema_ddl())?;
        conn.execute_batch("CALL lake.set_option('data_inlining_row_limit', 10000)")?;
        conn.register_table_function::<ArrowVTab>("arrow")?;
        Ok(Self {
            env,
            stores,
            conn,
            written_spans: vec![],
            counters: synth::counters(0, 0),
            written_samples: vec![],
            probe_trace: [0; 16],
            flushers: HashMap::new(),
            findings: vec![],
        })
    }

    fn record(&mut self, id: &'static str, claim: &'static str, pass: bool, evidence: Vec<String>) {
        println!("\n[{}] {id}: {claim}", if pass { "PASS" } else { "FAIL" });
        for e in &evidence {
            println!("    - {e}");
        }
        self.findings.push(Finding { id, claim, pass, evidence });
    }
}

/// E1: MinIO honours If-None-Match and If-Match, so the commit queue and the coordinator
/// lease need no database in single-node mode.
pub async fn e1_conditional_writes(ctx: &mut Ctx) -> Result<()> {
    let store = ctx.stores["us-east"].clone();
    let key = Path::from("catalog/default/us-east/coordinator.lease");
    let create = PutOptions { mode: PutMode::Create, ..Default::default() };
    let first = store.put_opts(&key, PutPayload::from_static(b"holder=a epoch=1"), create.clone()).await?;
    let second = store.put_opts(&key, PutPayload::from_static(b"holder=b epoch=1"), create.clone()).await;
    let second_refused = matches!(second, Err(object_store::Error::AlreadyExists { .. }));
    let v1 = UpdateVersion { e_tag: first.e_tag.clone(), version: None };
    let renew = store
        .put_opts(&key, PutPayload::from_static(b"holder=a epoch=2"), PutOptions { mode: PutMode::Update(v1.clone()), ..Default::default() })
        .await;
    let stale = store
        .put_opts(&key, PutPayload::from_static(b"holder=b epoch=2"), PutOptions { mode: PutMode::Update(v1), ..Default::default() })
        .await;
    let stale_refused = matches!(stale, Err(object_store::Error::Precondition { .. }));
    // Eight would-be coordinators race for a fresh lease: exactly one may win.
    let race_key = Path::from("catalog/default/us-east/race.lease");
    let mut tasks = vec![];
    for i in 0..8 {
        let (s, k, o) = (store.clone(), race_key.clone(), create.clone());
        tasks.push(tokio::spawn(async move { s.put_opts(&k, PutPayload::from(format!("holder={i}")), o).await.is_ok() }));
    }
    let mut winners = 0;
    for t in tasks {
        winners += t.await? as usize;
    }
    let pass = second_refused && renew.is_ok() && stale_refused && winners == 1;
    ctx.record(
        "E1",
        "Object storage (MinIO) gives the conditional writes the commit queue and lease rely on",
        pass,
        vec![
            format!("create-if-absent twice: second refused = {second_refused}"),
            format!("renew with current ETag accepted = {}", renew.is_ok()),
            format!("update with stale ETag refused (412) = {stale_refused}"),
            format!("8 concurrent create-if-absent on one lease key: {winners} winner(s)"),
        ],
    );
    Ok(())
}

/// E2: the ingest path writes sorted ZSTD Parquet and commit envelopes with no DuckDB and
/// no routing; a node crash loses only its unflushed batch.
pub async fn e2_ingest(ctx: &mut Ctx) -> Result<()> {
    let all_spans = synth::traces(40_000, 7);
    ctx.counters = synth::counters(2_000, 11);
    let crashed = ("us-east", 1usize, 1i64); // region, node, window it dies in
    let mut lost_spans = 0usize;
    let mut lost_samples = 0usize;
    let mut bytes_by_table: HashMap<&str, u64> = HashMap::new();
    let mut envelopes = 0;
    let mut files = 0;
    for region in REGIONS {
        for n in 0..NODES_PER_REGION {
            ctx.flushers.insert(
                (region, n),
                Flusher { region, node_id: format!("{region}-n{n}"), node_epoch: 1, seq: 0, store: ctx.stores[region].clone() },
            );
        }
    }
    let t_ingest = Instant::now();
    for w in 0..WINDOWS {
        for region in REGIONS {
            for n in 0..NODES_PER_REGION {
                // A load balancer sprays: placement is arbitrary, not owner-routed.
                let mut spans: Vec<Span> = all_spans
                    .iter()
                    .filter(|s| s.region == region && synth::window_of_ns(s.start_ns) == w && (s.span_id as usize >> 7) % NODES_PER_REGION == n)
                    .cloned()
                    .collect();
                let mut samples: Vec<synth::Sample> = ctx
                    .counters
                    .samples
                    .iter()
                    .filter(|s| {
                        s.region == region
                            && (s.ts_ms - synth::T0_MS) / synth::WINDOW_MS == w
                            && ((s.series_id ^ s.ts_ms as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 60) as usize % NODES_PER_REGION == n
                    })
                    .cloned()
                    .collect();
                if (region, n, w) == crashed {
                    lost_spans += spans.len();
                    lost_samples += samples.len();
                    let f = ctx.flushers.get_mut(&(region, n)).ok_or_else(|| anyhow!("flusher"))?;
                    f.node_epoch += 1; // restart: new epoch, seq restarts
                    f.seq = 0;
                    continue;
                }
                let flusher = ctx.flushers.get_mut(&(region, n)).ok_or_else(|| anyhow!("flusher"))?;
                let mut entries: Vec<FileEntry> = vec![];
                for (table, layout, bloom) in [
                    ("spans", SpanLayout::TraceSortedBloom, Some("trace_id")),
                    ("spans_nobloom", SpanLayout::TraceSortedNoBloom, None),
                    ("spans_v5", SpanLayout::ServiceFirstBloom, Some("trace_id")),
                ] {
                    let batch = write::span_batch(&mut spans, layout)?;
                    let data = write::parquet_bytes(&batch, bloom, 4_096)?;
                    *bytes_by_table.entry(table).or_default() += data.len() as u64;
                    entries.push(flusher.put_file(table, w, data, spans.len()).await?);
                }
                let batch = write::point_batch(&mut samples)?;
                let data = write::parquet_bytes(&batch, None, 65_536)?;
                *bytes_by_table.entry("metrics_points").or_default() += data.len() as u64;
                entries.push(flusher.put_file("metrics_points", w, data, samples.len()).await?);
                files += entries.len();
                flusher.commit(entries).await?;
                envelopes += 1;
                ctx.written_spans.extend(spans);
                ctx.written_samples.extend(samples);
            }
        }
    }
    let ingest_ms = ms(t_ingest);
    // Probe: a trace with every span written, spans in both regions and in two flush windows.
    let mut by_trace: HashMap<[u8; 16], Vec<&Span>> = HashMap::new();
    for s in &ctx.written_spans {
        by_trace.entry(s.trace_id).or_default().push(s);
    }
    let probe = by_trace
        .iter()
        .filter(|(_, v)| v.len() == 5)
        .filter(|(_, v)| v.iter().map(|s| s.region).collect::<HashSet<_>>().len() == 2)
        .filter(|(_, v)| v.iter().map(|s| synth::window_of_ns(s.start_ns + s.duration_ns as i64)).collect::<HashSet<_>>().len() >= 1
            && v.iter().map(|s| synth::window_of_ns(s.start_ns)).collect::<HashSet<_>>().len() == 2)
        .map(|(k, _)| *k)
        .min()
        .ok_or_else(|| anyhow!("generator produced no trace that crosses regions and a flush boundary"))?;
    ctx.probe_trace = probe;
    let samples_written = ctx.written_samples.len() as f64;
    ctx.record(
        "E2",
        "Ingest writes sorted ZSTD Parquet + commit envelopes with no DuckDB and no routing",
        envelopes > 0 && lost_spans > 0,
        vec![
            format!("{} spans and {} counter samples generated; {} spans and {} samples written", all_spans.len(), ctx.counters.samples.len(), ctx.written_spans.len(), ctx.written_samples.len()),
            format!("{envelopes} envelopes, {files} Parquet files PUT to 2 regional buckets in {ingest_ms} ms (includes 3 span layouts for E4)"),
            format!("crash of us-east-n1 in window 1 lost {lost_spans} spans and {lost_samples} samples (unflushed batch; durability mode `memory`)"),
            format!("bytes: spans {} / spans_nobloom {} / spans_v5 {} / metrics_points {}", bytes_by_table["spans"], bytes_by_table["spans_nobloom"], bytes_by_table["spans_v5"], bytes_by_table["metrics_points"]),
            format!("metrics L0 bytes/sample = {:.2} (small sprayed files, ~1,000 samples each)", bytes_by_table["metrics_points"] as f64 / samples_written),
            format!("probe trace {} crosses both regions and a flush boundary", hex(&probe)),
        ],
    );
    Ok(())
}

pub struct CkptStats {
    pub applied: usize,
    pub skipped: usize,
    pub latencies_ms: Vec<f64>,
}

/// The checkpointer: envelopes -> one DuckLake transaction each (files + idempotency row).
pub async fn checkpoint(ctx: &Ctx, keep_first_envelope: bool) -> Result<CkptStats> {
    let mut stats = CkptStats { applied: 0, skipped: 0, latencies_ms: vec![] };
    for region in REGIONS {
        let store = &ctx.stores[region];
        let prefix = Path::from(format!("commits/{region}"));
        let mut metas: Vec<_> = store.list(Some(&prefix)).try_collect().await?;
        metas.sort_by(|a, b| a.location.cmp(&b.location));
        for (i, meta) in metas.iter().enumerate() {
            let body = store.get_opts(&meta.location, object_store::GetOptions::default()).await?.bytes().await?;
            let env: Envelope = serde_json::from_slice(&body).with_context(|| format!("parsing {}", meta.location))?;
            let seen = count(&ctx.conn, &format!("SELECT count(*) FROM applied_envelopes WHERE envelope_id = {}", quote(&env.envelope_id)))?;
            if seen > 0 {
                stats.skipped += 1;
            } else {
                let t = Instant::now();
                apply_envelope(&ctx.conn, &env)?;
                stats.latencies_ms.push(ms(t));
                stats.applied += 1;
            }
            // An envelope is deleted only after the catalog commit that includes it. The
            // simulated crash keeps exactly one envelope: the first one of the first region.
            if !(keep_first_envelope && i == 0 && region == REGIONS[0]) {
                store.delete(&meta.location).await?;
            }
        }
    }
    Ok(stats)
}

pub fn apply_envelope(conn: &Connection, env: &Envelope) -> Result<()> {
    conn.execute_batch("BEGIN TRANSACTION")?;
    let body = || -> Result<()> {
        for f in &env.files {
            conn.execute_batch(&format!("CALL ducklake_add_data_files('lake', {}, {})", quote(&f.table), quote(&f.path)))?;
        }
        conn.execute_batch(&format!(
            "INSERT INTO applied_envelopes VALUES ({}, {}, {}, {}, {}, {})",
            quote(&env.envelope_id), quote(&env.region), quote(&env.node_id), env.node_epoch, env.seq_to, env.files.len()
        ))?;
        Ok(())
    };
    match body() {
        Ok(()) => Ok(conn.execute_batch("COMMIT")?),
        Err(e) => {
            conn.execute_batch("ROLLBACK")?;
            Err(e)
        }
    }
}

/// E3: one catalog for two buckets; envelope application is atomic and idempotent.
pub async fn e3_checkpoint(ctx: &mut Ctx) -> Result<()> {
    let first = checkpoint(ctx, true).await?;
    let second = checkpoint(ctx, false).await?;
    let spans = count(&ctx.conn, "SELECT count(*) FROM spans")?;
    let points = count(&ctx.conn, "SELECT count(*) FROM metrics_points")?;
    let regions = count(&ctx.conn, "SELECT count(DISTINCT region) FROM spans")?;
    // Atomicity: an envelope whose second file does not exist must leave nothing behind.
    let files_before = count(&ctx.conn, "SELECT count(*) FROM ducklake_list_files('lake', 'spans')")?;
    let bad = Envelope {
        format: 1, envelope_id: "bad-envelope".into(), kind: "ingest".into(), tenant: "default".into(),
        region: "us-east".into(), node_id: "x".into(), node_epoch: 1, seq_to: 1,
        files: vec![
            FileEntry { table: "spans".into(), path: ctx.written_path_example()?, bytes: 0, rows: 0 },
            FileEntry { table: "spans".into(), path: "s3://telemetry-us-east/spans/missing.parquet".into(), bytes: 0, rows: 0 },
        ],
    };
    let bad_result = apply_envelope(&ctx.conn, &bad);
    let files_after = count(&ctx.conn, "SELECT count(*) FROM ducklake_list_files('lake', 'spans')")?;
    let bad_marker = count(&ctx.conn, "SELECT count(*) FROM applied_envelopes WHERE envelope_id = 'bad-envelope'")?;
    let mut lat = first.latencies_ms.clone();
    lat.sort_by(f64::total_cmp);
    let pass = spans as usize == ctx.written_spans.len()
        && points as usize == ctx.written_samples.len()
        && second.applied == 0
        && second.skipped == 1
        && bad_result.is_err()
        && files_after == files_before
        && bad_marker == 0
        && regions == 2;
    ctx.record(
        "E3",
        "Checkpointer registers external files from two buckets atomically and idempotently",
        pass,
        vec![
            format!("pass 1 applied {} envelopes; commit latency p50 {} ms, max {} ms (4 files each, PostgreSQL catalog)", first.applied, lat[lat.len() / 2], lat[lat.len() - 1]),
            format!("pass 2 (after a simulated crash before envelope delete): applied {}, skipped {} — no duplicate rows", second.applied, second.skipped),
            format!("lake rows: spans {spans} (expected {}), metrics_points {points} (expected {}), regions {regions}", ctx.written_spans.len(), ctx.written_samples.len()),
            format!("envelope with a missing file: error = {}, files {files_before} -> {files_after}, marker rows {bad_marker}", bad_result.is_err()),
        ],
    );
    Ok(())
}

impl Ctx {
    fn written_path_example(&self) -> Result<String> {
        let p: String = self.conn.query_row("SELECT data_file FROM ducklake_list_files('lake', 'spans') LIMIT 1", [], |r| r.get(0))?;
        Ok(p)
    }
}

fn lookup(conn: &Connection, table: &str, trace: &[u8; 16]) -> Result<(i64, i64, u64, u64, f64)> {
    conn.execute_batch("CALL truncate_duckdb_logs()")?;
    let t = Instant::now();
    let (n, regions): (i64, i64) = conn.query_row(
        &format!("SELECT count(*), count(DISTINCT region) FROM {table} WHERE trace_id = from_hex({})", quote(&hex(trace))),
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let elapsed = ms(t);
    let (gets, bytes) = lake::http_gets(conn)?;
    Ok((n, regions, gets, bytes, elapsed))
}

/// E4: trace lookup by id prunes with sort order + bloom filters; cross-region stitching works.
pub async fn e4_trace_lookup(ctx: &mut Ctx) -> Result<()> {
    ctx.conn.execute_batch("CALL enable_logging('HTTP')")?;
    let expected = ctx.written_spans.iter().filter(|s| s.trace_id == ctx.probe_trace).count() as i64;
    let mut ev = vec![];
    let mut ok = true;
    let files = count(&ctx.conn, "SELECT count(*) FROM ducklake_list_files('lake', 'spans')")?;
    for table in ["spans", "spans_nobloom", "spans_v5"] {
        let (n, regions, gets, bytes, t) = lookup(&ctx.conn, table, &ctx.probe_trace)?;
        ok &= n == expected && regions == 2;
        ev.push(format!("{table:<14} {n}/{expected} spans from {regions} regions; {gets} GETs, {:.1} KiB read, {t} ms over {files} files", bytes as f64 / 1024.0));
    }
    // A trace id that does not exist shows the floor cost: footers + blooms, no data pages.
    let (n, _, gets, bytes, t) = lookup(&ctx.conn, "spans", &[0xAB; 16])?;
    ev.push(format!("absent trace id on `spans`: {n} rows; {gets} GETs, {:.1} KiB, {t} ms", bytes as f64 / 1024.0));
    let mut stmt = ctx.conn.prepare(&format!(
        "SELECT region, service_name, span_name, round(duration_ns / 1e6, 1)::VARCHAR AS ms, parent_span_id IS NULL AS root
         FROM spans WHERE trace_id = from_hex({}) ORDER BY start_time_ns", quote(&hex(&ctx.probe_trace))))?;
    let rows: Vec<String> = stmt
        .query_map([], |r| Ok(format!("{} {} {} {}ms{}", r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, if r.get::<_, bool>(4)? { " (root)" } else { "" })))?
        .collect::<Result<_, _>>()?;
    ev.push(format!("waterfall: {}", rows.join(" -> ")));
    ctx.record("E4", "Trace lookup by id across regions and flush boundaries, pruned by sort + bloom", ok, ev);
    Ok(())
}

fn counter_increases(conn: &Connection, delta_sql: &str) -> Result<HashMap<u64, f64>> {
    let mut stmt = conn.prepare(delta_sql)?;
    let out = stmt.query_map([], |r| Ok((r.get::<_, u64>(0)?, r.get::<_, f64>(1)?)))?.collect::<Result<HashMap<_, _>, _>>()?;
    Ok(out)
}

const LAG_INCREASE: &str = "
    WITH d AS (SELECT series_id, value, value - lag(value) OVER (PARTITION BY series_id ORDER BY timestamp_ms) AS diff
               FROM metrics_points)
    SELECT series_id, sum(CASE WHEN diff IS NULL THEN 0 WHEN diff >= 0 THEN diff ELSE value END) FROM d GROUP BY 1";

fn increase_of(samples: &[&synth::Sample]) -> f64 {
    samples.windows(2).map(|w| if w[1].value >= w[0].value { w[1].value - w[0].value } else { w[1].value }).sum()
}

/// E5: counter increase computed at read (L0) and by the compactor (L1) is exact for
/// everything that reached storage, with no ingest-side state; v5's ingest-state rule is not.
pub async fn e5_counters(ctx: &mut Ctx) -> Result<()> {
    let lake = counter_increases(&ctx.conn, LAG_INCREASE)?;
    // Best possible: the same rule applied to exactly the samples that reached storage.
    let mut by_series: HashMap<u64, Vec<&synth::Sample>> = HashMap::new();
    for s in &ctx.written_samples {
        by_series.entry(s.series_id).or_default().push(s);
    }
    for v in by_series.values_mut() {
        v.sort_by_key(|s| s.ts_ms);
    }
    let best: HashMap<u64, f64> = by_series.iter().map(|(k, v)| (*k, increase_of(v))).collect();
    let truth = &ctx.counters.true_increase;
    let lake_vs_best = best.iter().filter(|(k, v)| (lake.get(k).copied().unwrap_or(f64::NAN) - **v).abs() > 1e-9).count();
    let total_truth: f64 = truth.values().sum();
    let total_lake: f64 = lake.values().sum();
    // v5 as written: owner-routed series, ingest-time delta from in-memory last point; the
    // crashed owner loses its window-1 samples AND its state, so its first post-restart
    // delta is 0.
    let mut v5_total = 0.0;
    let mut all_by_series: HashMap<u64, Vec<&synth::Sample>> = HashMap::new();
    for s in &ctx.counters.samples {
        all_by_series.entry(s.series_id).or_default().push(s);
    }
    for (sid, v) in all_by_series.iter_mut() {
        v.sort_by_key(|s| s.ts_ms);
        let owner_crashed = v[0].region == "us-east" && sid % NODES_PER_REGION as u64 == 1;
        let mut last: Option<f64> = None;
        for s in v.iter() {
            let window = (s.ts_ms - synth::T0_MS) / synth::WINDOW_MS;
            if owner_crashed && window == 1 {
                last = None; // samples lost with the node, state gone with it
                continue;
            }
            if let Some(p) = last {
                v5_total += if s.value >= p { s.value - p } else { s.value };
            }
            last = Some(s.value);
        }
    }
    // Compactor: single writer per (region, partition) computes positive_delta into L1 and
    // swaps L0 -> L1 in one catalog transaction.
    let t = Instant::now();
    let mut l1_bytes = 0u64;
    for region in REGIONS {
        let l0: Vec<String> = {
            let mut stmt = ctx.conn.prepare(&format!(
                "SELECT data_file FROM ducklake_list_files('lake', 'metrics_points') WHERE data_file LIKE 's3://telemetry-{region}/%'"))?;
            stmt.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?
        };
        let l1 = format!("s3://telemetry-{region}/metrics_points/tenant=default/l1-{}-compacted.parquet", ulid::Ulid::new());
        ctx.conn.execute_batch(&format!(
            "COPY (SELECT tenant, region, metric_name, series_id, timestamp_ms, value,
                     CASE WHEN lag(value) OVER w IS NULL THEN 0
                          WHEN value >= lag(value) OVER w THEN value - lag(value) OVER w
                          ELSE value END AS positive_delta
                   FROM metrics_points WHERE region = {r}
                   WINDOW w AS (PARTITION BY series_id ORDER BY timestamp_ms)
                   ORDER BY series_id, timestamp_ms)
             TO {l1} (FORMAT parquet, COMPRESSION zstd, ROW_GROUP_SIZE 122880)",
            r = quote(region), l1 = quote(&l1)))?;
        let list = l0.iter().map(|p| quote(p)).collect::<Vec<_>>().join(", ");
        ctx.conn.execute_batch(&format!(
            "BEGIN TRANSACTION;
             DELETE FROM metrics_points WHERE filename IN ({list});
             CALL ducklake_add_data_files('lake', 'metrics_points', {});
             COMMIT;", quote(&l1)))?;
        l1_bytes += ctx.conn.query_row(&format!("SELECT data_file_size_bytes FROM ducklake_list_files('lake','metrics_points') WHERE data_file = {}", quote(&l1)), [], |r| r.get::<_, i64>(0))? as u64;
    }
    let compact_ms = ms(t);
    let l1 = counter_increases(&ctx.conn, "SELECT series_id, sum(positive_delta) FROM metrics_points GROUP BY 1")?;
    let l1_vs_best = best.iter().filter(|(k, v)| (l1.get(k).copied().unwrap_or(f64::NAN) - **v).abs() > 1e-9).count();
    let reset_in_lost = best.iter().filter(|(k, v)| (truth[*k] - **v).abs() > 1e-9).count();
    let pass = lake_vs_best == 0 && l1_vs_best == 0;
    let pct = |x: f64| (x - total_truth) / total_truth * 100.0;
    ctx.record(
        "E5",
        "Counter increase needs no ingest-side state: computed at read (L0) and by the compactor (L1)",
        pass,
        vec![
            format!("{} series; true total increase {total_truth:.0}", truth.len()),
            format!("read-time lag over L0: total {total_lake:.0} ({:+.4} %); series differing from best-possible: {lake_vs_best}", pct(total_lake)),
            format!("compactor L1 `positive_delta`: series differing from best-possible: {l1_vs_best}; L1 {:.2} bytes/sample; compaction {compact_ms} ms", l1_bytes as f64 / ctx.written_samples.len() as f64),
            format!("series that lost increase physically (a reset inside the crashed batch): {reset_in_lost}"),
            format!("v5 ingest-state rule under the same crash: total {v5_total:.0} ({:+.4} %)", pct(v5_total)),
        ],
    );
    Ok(())
}

/// E6: our own compactor (Rust writer: sort + bloom) swaps L0 -> L1 atomically; the change
/// feed reports the swap as inserts+deletes; DuckLake cleanup and external files.
pub async fn e6_compaction(ctx: &mut Ctx) -> Result<()> {
    let before = count(&ctx.conn, "SELECT count(*) FROM spans")?;
    let (_, _, gets_before, bytes_before, _) = lookup(&ctx.conn, "spans", &ctx.probe_trace)?;
    let snap_before = count(&ctx.conn, "SELECT id FROM ducklake_current_snapshot('lake')")?;
    let mut swapped_l0: Vec<String> = vec![];
    for region in REGIONS {
        let l0: Vec<String> = {
            let mut stmt = ctx.conn.prepare(&format!("SELECT data_file FROM ducklake_list_files('lake', 'spans') WHERE data_file LIKE 's3://telemetry-{region}/%'"))?;
            stmt.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?
        };
        let batches: Vec<RecordBatch> = ctx
            .conn
            .prepare(&format!("SELECT * FROM spans WHERE region = {} ORDER BY trace_id, start_time_ns", quote(region)))?
            .query_arrow([])?
            .collect();
        let batch = arrow::compute::concat_batches(&batches[0].schema(), &batches)?;
        let data = write::parquet_bytes(&batch, Some("trace_id"), 8_192)?;
        let key = format!("spans/tenant=default/l1-{}-compacted.parquet", ulid::Ulid::new());
        ctx.stores[region].put(&Path::from(key.as_str()), PutPayload::from(data)).await?;
        let l1 = format!("s3://telemetry-{region}/{key}");
        let list = l0.iter().map(|p| quote(p)).collect::<Vec<_>>().join(", ");
        ctx.conn.execute_batch(&format!(
            "BEGIN TRANSACTION; DELETE FROM spans WHERE filename IN ({list}); CALL ducklake_add_data_files('lake', 'spans', {}); COMMIT;",
            quote(&l1)))?;
        swapped_l0.extend(l0);
    }
    let after = count(&ctx.conn, "SELECT count(*) FROM spans")?;
    let snap_after = count(&ctx.conn, "SELECT id FROM ducklake_current_snapshot('lake')")?;
    let feed_ins = count(&ctx.conn, &format!("SELECT count(*) FROM ducklake_table_insertions('lake', 'main', 'spans', {}, {snap_after})", snap_before + 1))?;
    let feed_del = count(&ctx.conn, &format!("SELECT count(*) FROM ducklake_table_deletions('lake', 'main', 'spans', {}, {snap_after})", snap_before + 1))?;
    let (n, _, gets_after, bytes_after, t_after) = lookup(&ctx.conn, "spans", &ctx.probe_trace)?;
    // Does DuckLake reclaim external L0 files once no snapshot references them?
    ctx.conn.execute_batch("CALL ducklake_expire_snapshots('lake', older_than => now()); CALL ducklake_cleanup_old_files('lake', cleanup_all => true);")?;
    let mut remaining = 0;
    for p in &swapped_l0 {
        let (bucket_region, key) = p.trim_start_matches("s3://telemetry-").split_once('/').ok_or_else(|| anyhow!("path {p}"))?;
        if ctx.stores[bucket_region].head(&Path::from(key)).await.is_ok() {
            remaining += 1;
        }
    }
    ctx.record(
        "E6",
        "Own compactor swaps L0->L1 atomically; change feed is unsafe as a view source",
        before == after && n > 0,
        vec![
            format!("rows before/after swap: {before}/{after}; {} L0 files replaced by 2 L1 files", swapped_l0.len()),
            format!("probe lookup: {gets_before} GETs / {:.1} KiB before, {gets_after} GETs / {:.1} KiB / {t_after} ms after", bytes_before as f64 / 1024.0, bytes_after as f64 / 1024.0),
            format!("change feed over the swap: {feed_ins} insertions + {feed_del} deletions for 0 net new rows"),
            format!("after expire_snapshots + cleanup_old_files: {remaining}/{} externally written L0 objects still in MinIO", swapped_l0.len()),
        ],
    );
    Ok(())
}

/// E7: an unflushed Arrow batch on a node is queryable with the lake, and the per-node
/// watermark keeps every state of flush/commit/drop from double counting.
pub async fn e7_hot_tail(ctx: &mut Ctx) -> Result<()> {
    let mut hot = synth::traces(20_000, 99).into_iter().filter(|s| s.region == "us-east").collect::<Vec<_>>();
    let truth_new = hot.len() as i64;
    let batch = write::span_batch(&mut hot, SpanLayout::TraceSortedBloom)?;
    let base = count(&ctx.conn, "SELECT count(*) FROM spans")?;
    let flusher = ctx.flushers.get_mut(&("us-east", 0)).ok_or_else(|| anyhow!("flusher"))?;
    let (node, epoch, hot_seq) = (flusher.node_id.clone(), flusher.node_epoch, flusher.seq + 1);
    let reg = ArrowBatchRegistration::new(batch.clone());
    let hot_sql = "SELECT count(*) FROM (SELECT trace_id FROM spans UNION ALL SELECT trace_id FROM arrow(?))";
    // One read transaction pins the snapshot that both the watermark and the scan see.
    let query = |conn: &Connection, retained: bool| -> Result<(i64, f64)> {
        conn.execute_batch("BEGIN TRANSACTION")?;
        let w: i64 = conn.query_row(
            &format!("SELECT coalesce(max(seq_to), 0) FROM applied_envelopes WHERE node_id = {} AND node_epoch = {epoch}", quote(&node)),
            [], |r| r.get(0))?;
        let t = Instant::now();
        let n = if retained && hot_seq > w {
            conn.query_row(hot_sql, [&reg], |r| r.get(0))?
        } else {
            count(conn, "SELECT count(*) FROM spans")?
        };
        let elapsed = ms(t);
        conn.execute_batch("COMMIT")?;
        Ok((n, elapsed))
    };
    let (a, t_a) = query(&ctx.conn, true)?;
    // Flush + commit the same batch, but keep it in RAM (grace period).
    let data = write::parquet_bytes(&batch, Some("trace_id"), 8_192)?;
    let entry = ctx.flushers.get_mut(&("us-east", 0)).ok_or_else(|| anyhow!("flusher"))?.put_file("spans", 3, data, hot.len()).await?;
    ctx.flushers.get_mut(&("us-east", 0)).ok_or_else(|| anyhow!("flusher"))?.commit(vec![entry]).await?;
    checkpoint(ctx, false).await?;
    let (b, t_b) = query(&ctx.conn, true)?;
    let naive: i64 = ctx.conn.query_row(hot_sql, [&reg], |r| r.get(0))?;
    let (c, _) = query(&ctx.conn, false)?; // the node dropped the batch after the grace period
    let expected = base + truth_new;
    let t = Instant::now();
    count(&ctx.conn, "SELECT count(*) FROM spans")?;
    let lake_only_ms = ms(t);
    ctx.record(
        "E7",
        "Hot-tail Arrow batch unions with the lake; per-node watermark prevents double count",
        a == expected && b == expected && c == expected && naive == expected + truth_new,
        vec![
            format!("hot batch: {truth_new} spans (node {node} epoch {epoch} seq {hot_seq})"),
            format!("A before flush (hot only): {a} (expected {expected}), query {t_a} ms"),
            format!("B committed, still retained in RAM: {b} (expected {expected}), query {t_b} ms"),
            format!("B without the watermark (naive UNION ALL): {naive} — double counts {}", naive - expected),
            format!("C dropped from RAM: {c} (expected {expected}); lake-only count {lake_only_ms} ms"),
        ],
    );
    Ok(())
}

/// E8: percentiles from log-bucket partials merged by SUM in SQL; partial rows are inlined
/// in the catalog.
pub async fn e8_percentiles(ctx: &mut Ctx) -> Result<()> {
    const SCALE: f64 = 32.0; // 2^(1/32) bucket width: <= ~1.1 % relative error at the midpoint
    ctx.conn.execute_batch(&format!(
        "INSERT INTO span_hist_partials
         SELECT region, filename, service_name, ceil(log2(duration_ns / 1000.0) * {SCALE})::SMALLINT, count(*)
         FROM spans GROUP BY ALL"))?;
    let partial_rows = count(&ctx.conn, "SELECT count(*) FROM span_hist_partials")?;
    let partial_files = count(&ctx.conn, "SELECT count(*) FROM ducklake_list_files('lake', 'span_hist_partials')")?;
    let merged_sql = format!(
        "WITH m AS (SELECT service_name, bucket, sum(n) AS n FROM span_hist_partials GROUP BY ALL),
              c AS (SELECT service_name, bucket, sum(n) OVER (PARTITION BY service_name ORDER BY bucket) AS cum,
                           sum(n) OVER (PARTITION BY service_name) AS tot FROM m)
         SELECT service_name, q, 1000 * pow(2, (min(bucket) - 0.5) / {SCALE}) AS est
         FROM c, (VALUES (0.5), (0.95), (0.99)) qs(q) WHERE cum >= q * tot GROUP BY ALL ORDER BY ALL");
    let mut lat = vec![];
    let mut est: Vec<(String, f64, f64)> = vec![];
    for _ in 0..20 {
        let t = Instant::now();
        let mut stmt = ctx.conn.prepare(&merged_sql)?;
        est = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?, r.get::<_, f64>(2)?)))?.collect::<Result<_, _>>()?;
        lat.push(ms(t));
    }
    lat.sort_by(f64::total_cmp);
    let mut worst: f64 = 0.0;
    for (svc, q, e) in &est {
        let exact: f64 = ctx.conn.query_row(
            &format!("SELECT quantile_cont(duration_ns, {q}) FROM spans WHERE service_name = {}", quote(svc)), [], |r| r.get(0))?;
        worst = worst.max(((e - exact) / exact).abs());
    }
    ctx.record(
        "E8",
        "Mergeable percentiles: log-bucket partials merged with SUM in plain SQL",
        worst < 0.02,
        vec![
            format!("{partial_rows} partial rows from {} files x 4 services; parquet files for partials: {partial_files} (0 = inlined in the catalog)", count(&ctx.conn, "SELECT count(*) FROM ducklake_list_files('lake','spans')")?),
            format!("p50/p95/p99 for 4 services, merged across regions and files: worst relative error {:.2} % vs exact", worst * 100.0),
            format!("panel read (merge + quantiles) over inlined partials: p50 {} ms, max {} ms (20 runs)", lat[lat.len() / 2], lat[lat.len() - 1]),
        ],
    );
    Ok(())
}

/// E9: a laptop reads production through a catalog snapshot in object storage, read-only,
/// with no database connection and no load on the cluster.
pub async fn e9_laptop(ctx: &mut Ctx) -> Result<()> {
    let snap_path = std::env::current_dir()?.join(".state/snapshot-latest.ducklake");
    let _ = std::fs::remove_file(&snap_path);
    // The PostgreSQL backend stores inlined rows in its own column types (bytea for VARCHAR,
    // varchar for UBIGINT/TIMESTAMP/MAP), so a one-to-one copy of an inlined-data table is not
    // a valid DuckDB-backend catalog. Flush inlined rows to Parquet first, then copy only the
    // metadata tables and declare that the snapshot holds no inlined data.
    let inlined_before = count(&ctx.conn, "SELECT count(*) FROM ducklake_list_files('lake', 'span_hist_partials')")?;
    ctx.conn.execute_batch("CALL ducklake_flush_inlined_data('lake')")?;
    let inlined_after = count(&ctx.conn, "SELECT count(*) FROM ducklake_list_files('lake', 'span_hist_partials')")?;
    ctx.conn.execute_batch(&format!(
        "ATTACH {} AS pgcat (TYPE postgres, READ_ONLY); ATTACH {} AS snap;",
        quote(&ctx.env.pg), quote(&snap_path.to_string_lossy())))?;
    let tables: Vec<String> = {
        let mut stmt = ctx.conn.prepare(
            "SELECT table_name FROM duckdb_tables() WHERE database_name = 'pgcat' AND schema_name = 'public'
             AND table_name LIKE 'ducklake%' AND table_name NOT LIKE 'ducklake_inlined_data_%'")?;
        stmt.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?
    };
    for t in &tables {
        ctx.conn.execute_batch(&format!("CREATE TABLE snap.{t} AS SELECT * FROM pgcat.public.{t}"))?;
    }
    ctx.conn.execute_batch(
        "CREATE TABLE snap.ducklake_inlined_data_tables AS SELECT * FROM pgcat.public.ducklake_inlined_data_tables WITH NO DATA;
         DETACH snap; DETACH pgcat;")?;
    let data = std::fs::read(&snap_path)?;
    let snap_bytes = data.len();
    ctx.stores["us-east"].put(&Path::from("catalog/default/snapshot-latest.ducklake"), PutPayload::from(data)).await?;

    let laptop = ctx.env.duckdb()?;
    let t = Instant::now();
    laptop.execute_batch("ATTACH 'ducklake:s3://telemetry-us-east/catalog/default/snapshot-latest.ducklake' AS lake (READ_ONLY); USE lake;")?;
    let attach_ms = ms(t);
    let t = Instant::now();
    let (n, regions): (i64, i64) = laptop.query_row(
        &format!("SELECT count(*), count(DISTINCT region) FROM spans WHERE trace_id = from_hex({})", quote(&hex(&ctx.probe_trace))), [], |r| Ok((r.get(0)?, r.get(1)?)))?;
    let q_ms = ms(t);
    let expected = count(&ctx.conn, &format!("SELECT count(*) FROM spans WHERE trace_id = from_hex({})", quote(&hex(&ctx.probe_trace))))?;
    let laptop_total: f64 = laptop.query_row("SELECT sum(positive_delta) FROM metrics_points", [], |r| r.get(0))?;
    let cluster_total: f64 = ctx.conn.query_row("SELECT sum(positive_delta) FROM metrics_points", [], |r| r.get(0))?;
    let write = laptop.execute_batch("INSERT INTO applied_envelopes VALUES ('x','x','x',0,0,0)");
    ctx.record(
        "E9",
        "Laptop/agent mode: read-only DuckLake catalog snapshot attached straight from S3",
        n == expected && regions == 2 && (laptop_total - cluster_total).abs() < 1e-6 && write.is_err(),
        vec![
            format!("inlined rows must be flushed first (PostgreSQL stores them in backend-specific types): partial files {inlined_before} -> {inlined_after}"),
            format!("snapshot = {} metadata tables copied from PostgreSQL into one {snap_bytes}-byte DuckDB file, PUT to MinIO", tables.len()),
            format!("fresh process, no PostgreSQL: attach {attach_ms} ms; trace lookup {n}/{expected} spans from {regions} regions in {q_ms} ms"),
            format!("counter total equals the cluster's: {}", (laptop_total - cluster_total).abs() < 1e-6),
            format!("write attempt refused: {}", write.is_err()),
        ],
    );
    Ok(())
}

pub async fn run_all(env: Env) -> Result<Vec<Finding>> {
    let mut ctx = Ctx::new(env)?;
    let existing = count(&ctx.conn, "SELECT count(*) FROM spans")?;
    if existing != 0 {
        bail!("the lake is not empty ({existing} spans): run `scripts/stack.sh reset` before `poc all`");
    }
    e1_conditional_writes(&mut ctx).await?;
    e2_ingest(&mut ctx).await?;
    e3_checkpoint(&mut ctx).await?;
    e4_trace_lookup(&mut ctx).await?;
    e5_counters(&mut ctx).await?;
    e6_compaction(&mut ctx).await?;
    e7_hot_tail(&mut ctx).await?;
    e8_percentiles(&mut ctx).await?;
    e9_laptop(&mut ctx).await?;
    Ok(ctx.findings)
}
