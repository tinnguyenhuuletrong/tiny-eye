//! `tiny-eye-fleet`: proof of concept for share-nothing nodes and a read-only viewer.
//!
//!   tiny-eye-fleet [--regions 3] [--series 500] [--hours 6] [--traces 6000] [--growth 240]
//!                  [--rtt 0,50,150] [--s3 http://127.0.0.1:19000] [--out report.md]
//!
//! The design under test: every node is today's single node with its own DuckLake catalog file
//! and its own object prefix; it publishes a copy of its catalog to object storage. A viewer
//! (the laptop) attaches every published catalog read-only and queries them as one through
//! UNION ALL views. No coordinator, no shared catalog, no lease.
//!
//! Phases:
//!   1. N region nodes and one baseline node (every region's data in one catalog) get the same
//!      remote-write samples and OTLP traces; all flush, checkpoint and compact.
//!   2. Each region publishes its catalog: COPY FROM DATABASE into a DuckDB file, PUT.
//!   3. The viewer attaches the N snapshots, (a) straight from object storage and (b) downloaded
//!      to local disk first, with object storage `--rtt` ms away (a delaying TCP proxy).
//!   4. Dashboard panels and trace lookups on the viewer against the baseline: equal results,
//!      first-query and warm latency, GET requests with caches off.
//!   5. Freshness: one new sample through flush, checkpoint, publish, viewer refresh.
//!   6. Catalog growth: many small flushes on one node, snapshot size and publish time, then
//!      compaction and snapshot expiry.

#![allow(clippy::unwrap_used)]

mod shared;

use anyhow::{Context, Result, anyhow};
use duckdb::Connection;
use engine::node::now_ms;
use engine::tables::Table;
use engine::{Config, Node, duck};
use prost::Message;
use rand::{SeedableRng, rngs::StdRng};
use shared::*;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

const SNAPSHOT_KEY: &str = "catalog/snapshot-latest.ducklake";

struct Args {
    regions: usize,
    series: usize,
    hours: i64,
    traces: usize,
    growth: usize,
    rtt: Vec<u64>,
    s3: Option<String>,
    out: Option<String>,
}

fn args() -> Args {
    let a: Vec<String> = std::env::args().collect();
    let get = |n: &str| a.iter().position(|x| x == n).and_then(|i| a.get(i + 1)).cloned();
    Args {
        regions: get("--regions").and_then(|v| v.parse().ok()).unwrap_or(3),
        series: get("--series").and_then(|v| v.parse().ok()).unwrap_or(500),
        hours: get("--hours").and_then(|v| v.parse().ok()).unwrap_or(6),
        traces: get("--traces").and_then(|v| v.parse().ok()).unwrap_or(6000),
        growth: get("--growth").and_then(|v| v.parse().ok()).unwrap_or(240),
        rtt: get("--rtt").unwrap_or_else(|| "0,50,150".into()).split(',').filter_map(|v| v.trim().parse().ok()).collect(),
        s3: get("--s3"),
        out: get("--out"),
    }
}

fn ms_since(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

/// One node in process behind its HTTP API.
struct Running {
    name: String,
    node: Arc<Node>,
    base: String,
}

async fn start(name: &str, region: &str, dir: &Path, storage: &str, ext: &str) -> Result<Running> {
    let d = dir.join(name);
    let d = d.display();
    let yaml = format!(
        "node: {{ id: {name}-n0, region: {region} }}\nduckdb: {{ extension_dir: {ext}, threads: 4, memory_limit: 4GB }}\n{storage}\n\
         catalog: {{ url: {d}/catalog.ducklake }}\n\
         ingest: {{ wal_dir: {d}/wal, max_sample_age: 30d, flush_interval: 1h, arena_limit: 4GB }}\nfleet: {{ dir: {d}/fleet }}\nmaterialize: {{ path: '' }}\n\
         coordinator: {{ hot_grace: 1ms, compaction_delay: 1ms, checkpoint_interval: 1h, compaction_interval: 1w, retention_interval: 1w }}\n\
         query: {{ workers: 2 }}\n"
    );
    let node = Node::open(Config::from_layers(Some(&yaml), &[])?).await?;
    let state = server::AppState::new(node.clone(), vec![])?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    tokio::spawn(async move { axum::serve(listener, server::app(state)).await });
    Ok(Running { name: name.into(), node, base })
}

struct Published {
    copy_ms: f64,
    gzip_ms: f64,
    put_ms: f64,
    bytes: usize,
    gz_bytes: usize,
}

/// Copy the node's DuckLake metadata into a fresh DuckDB file and PUT it next to the data, as is
/// (for attaching over HTTP) and gzipped (for downloading: the file is mostly empty blocks).
async fn publish(r: &Running, scratch: &Path) -> Result<Published> {
    let path = scratch.join(format!("{}-{}.ducklake", r.name, ulid::Ulid::generate()));
    let p = path.display().to_string();
    let t = Instant::now();
    r.node
        .with_conn(move |c| {
            c.execute_batch(&format!("ATTACH '{p}' AS snap; COPY FROM DATABASE __ducklake_metadata_lake TO snap; DETACH snap;"))?;
            Ok(())
        })
        .await?;
    let copy_ms = ms_since(t);
    let bytes = std::fs::read(&path)?;
    std::fs::remove_file(&path)?;
    let t = Instant::now();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    std::io::Write::write_all(&mut gz, &bytes)?;
    let gz = gz.finish()?;
    let gzip_ms = ms_since(t);
    let (n, gz_bytes) = (bytes.len(), gz.len());
    let t = Instant::now();
    r.node.store.put(SNAPSHOT_KEY, bytes.into()).await?;
    r.node.store.put(&format!("{SNAPSHOT_KEY}.gz"), gz.into()).await?;
    let put_ms = ms_since(t);
    Ok(Published { copy_ms, gzip_ms, put_ms, bytes: n, gz_bytes })
}

/// A TCP proxy that holds every chunk for `one_way` in each direction, chunks still pipelined:
/// object storage as seen from a laptop one round trip (2 × `one_way`) away.
async fn delay_proxy(upstream: String, one_way: std::time::Duration) -> Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    async fn pipe(mut r: tokio::net::tcp::OwnedReadHalf, mut w: tokio::net::tcp::OwnedWriteHalf, delay: std::time::Duration) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(tokio::time::Instant, Vec<u8>)>();
        tokio::spawn(async move {
            while let Some((at, buf)) = rx.recv().await {
                tokio::time::sleep_until(at).await;
                if w.write_all(&buf).await.is_err() {
                    break;
                }
            }
            let _ = w.shutdown().await;
        });
        let mut buf = vec![0u8; 64 * 1024];
        while let Ok(n @ 1..) = r.read(&mut buf).await {
            if tx.send((tokio::time::Instant::now() + delay, buf[..n].to_vec())).is_err() {
                break;
            }
        }
    }
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = l.local_addr()?;
    tokio::spawn(async move {
        while let Ok((client, _)) = l.accept().await {
            let up = upstream.clone();
            tokio::spawn(async move {
                let Ok(server) = tokio::net::TcpStream::connect(&up).await else { return };
                let _ = (client.set_nodelay(true), server.set_nodelay(true));
                let ((cr, cw), (sr, sw)) = (client.into_split(), server.into_split());
                tokio::spawn(pipe(cr, sw, one_way));
                tokio::spawn(pipe(sr, cw, one_way));
            });
        }
    });
    Ok(format!("http://{addr}"))
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// `ATTACH 'ducklake:s3://…/snapshot-latest.ducklake' (READ_ONLY)`: DuckDB reads the catalog over HTTP.
    Remote,
    /// GET the snapshot to local disk, then attach the local file.
    Download,
}

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Mode::Remote => "attached from object storage",
            Mode::Download => "downloaded, attached locally",
        }
    }
}

/// The laptop: one DuckDB with every region's snapshot attached and UNION ALL views over them,
/// named like the node's views so the TQS compiler runs unchanged. It reaches object storage
/// through its own endpoint (the delay proxy when a round trip is simulated).
struct Viewer {
    conn: Connection,
    store: engine::store::Store,
    mode: Mode,
    dir: PathBuf,
    generation: usize,
    attached: Vec<String>,
}

impl Viewer {
    fn open(ext: &str, storage_yaml: &str, s3: Option<&duck::S3Secret>, mode: Mode, dir: PathBuf) -> Result<Viewer> {
        let conn = duck::open(&duck::DuckSettings { extension_dir: ext.into(), threads: 4, memory_limit: "4GB".into() })?;
        if let Some(s) = s3 {
            duck::add_s3_secret(&conn, s)?;
        }
        let store = engine::store::Store::from_config(&Config::from_layers(Some(storage_yaml), &[])?)?;
        std::fs::create_dir_all(&dir)?;
        Ok(Viewer { conn, store, mode, dir, generation: 0, attached: vec![] })
    }

    /// Attach the latest snapshot of every region under new aliases, swap the views to them,
    /// then detach the previous generation. Returns ms spent downloading and attaching.
    async fn refresh(&mut self, regions: &[Running]) -> Result<(f64, f64)> {
        self.generation += 1;
        let mut aliases = vec![];
        let mut download_ms = 0.0;
        let mut locations = vec![];
        for r in regions {
            let alias = format!("{}_g{}", r.name.replace('-', "_"), self.generation);
            let loc = match self.mode {
                // Each node's storage.url is `<root>/<name>`; its objects are under `<region>/<id>/` there.
                Mode::Remote => self.store.url(&format!("{}/{}/{SNAPSHOT_KEY}", r.name, r.node.cfg.node.prefix())),
                Mode::Download => {
                    let t = Instant::now();
                    let gz = self.store.get(&format!("{}/{}/{SNAPSHOT_KEY}.gz", r.name, r.node.cfg.node.prefix())).await?;
                    let mut bytes = vec![];
                    std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(&gz[..]), &mut bytes)?;
                    let p = self.dir.join(format!("{alias}.ducklake"));
                    std::fs::write(&p, &bytes)?;
                    download_ms += ms_since(t);
                    p.display().to_string()
                }
            };
            locations.push((alias.clone(), loc));
            aliases.push(alias);
        }
        let t = Instant::now();
        for (alias, loc) in &locations {
            self.conn
                .execute_batch(&format!("ATTACH 'ducklake:{loc}' AS {alias} (READ_ONLY)"))
                .with_context(|| format!("attaching {loc}"))?;
        }
        for t in Table::ALL {
            let cols = t.column_names().join(", ");
            let n = t.name();
            let union = aliases.iter().map(|a| format!("SELECT {cols} FROM {a}.{n}")).collect::<Vec<_>>().join(" UNION ALL ");
            let body = match t {
                Table::MetricsSeries => format!(
                    "SELECT series_id, any_value(metric_name) AS metric_name, any_value(metric_type) AS metric_type, any_value(labels) AS labels FROM ({union}) GROUP BY series_id"
                ),
                _ => union,
            };
            self.conn.execute_batch(&format!("CREATE OR REPLACE VIEW memory.main.{n} AS {body}"))?;
        }
        self.conn.execute_batch("USE memory.main")?;
        for old in std::mem::replace(&mut self.attached, aliases) {
            self.conn.execute_batch(&format!("DETACH {old}"))?;
            let _ = std::fs::remove_file(self.dir.join(format!("{old}.ducklake")));
        }
        Ok((download_ms, ms_since(t)))
    }
}

/// Sorted result rows of a TQS document on a connection with the node's view names.
fn run_doc(c: &Connection, json: &str) -> Result<Vec<(String, String, i64, f64)>> {
    let (doc, r) = Node::parse_and_validate(json, now_ms()).map_err(|p| anyhow!("{p:?}"))?;
    let mut out: Vec<(String, String, i64, f64)> = match doc.kind {
        tqs::Kind::Timeseries | tqs::Kind::Table => {
            tqs::compile::run(c, &doc, &r)?.into_iter().map(|o| (o.output, o.key, o.t, o.value)).collect()
        }
        // `region` differs by design: the baseline node stamps its own ("all"), the viewer shows the source region.
        _ => tqs::compile::run_list(c, &doc, &r)?
            .into_iter()
            .map(|cells| {
                let cells: Vec<_> = cells.into_iter().filter(|(k, _)| k != "region").collect();
                (format!("{cells:?}"), String::new(), 0, 0.0)
            })
            .collect(),
    };
    out.sort_by(|a, b| (&a.0, &a.1, a.2).cmp(&(&b.0, &b.1, b.2)));
    Ok(out)
}

fn equal(a: &[(String, String, i64, f64)], b: &[(String, String, i64, f64)]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| x.0 == y.0 && x.1 == y.1 && x.2 == y.2 && (x.3 - y.3).abs() <= 1e-9 * x.3.abs().max(y.3.abs()).max(1.0))
}

struct Measure {
    rows: Vec<(String, String, i64, f64)>,
    first_ms: f64,
    p50: f64,
    gets: i64,
}

/// First run (cold for this connection), then the p50 of 10 warm runs, then GETs with caches off.
fn measure(c: &Connection, json: &str, count_gets: bool) -> Result<Measure> {
    let t = Instant::now();
    let rows = run_doc(c, json)?;
    let first_ms = ms_since(t);
    let mut ms = vec![];
    for _ in 0..10 {
        let t = Instant::now();
        run_doc(c, json)?;
        ms.push(ms_since(t));
    }
    let gets = if count_gets {
        c.execute_batch(
            "SET enable_external_file_cache = false; SET parquet_metadata_cache = false; CALL enable_logging('HTTP'); CALL truncate_duckdb_logs();",
        )?;
        run_doc(c, json)?;
        let g: i64 = c.query_row("SELECT count(*) FROM duckdb_logs WHERE type = 'HTTP' AND message LIKE '%GET%'", [], |r| r.get(0))?;
        c.execute_batch("CALL disable_logging(); SET enable_external_file_cache = true; SET parquet_metadata_cache = true;")?;
        g
    } else {
        -1
    };
    Ok(Measure { rows, first_ms, p50: pct(&mut ms, 0.5), gets })
}

async fn measure_node(r: &Running, json: String, count_gets: bool) -> Result<Measure> {
    r.node.with_conn(move |c| measure(c, &json, count_gets)).await
}

/// Each region's series carry `cluster=<region>`, like Prometheus external labels, so the same
/// label set never comes from two regions.
fn region_series(n: usize, region: &str) -> Vec<Vec<server::receivers::Label>> {
    let mut sets = series_set(n);
    for s in &mut sets {
        s.push(label("cluster", region));
    }
    sets
}

fn main() -> Result<()> {
    let a = args();
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(run(a))
}

async fn run(a: Args) -> Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let ext = root.join(".tools/duckdb_ext").display().to_string();
    let tmp = tempfile::tempdir()?;
    let dir = tmp.path();
    let run_id = ulid::Ulid::generate().to_string().to_lowercase();
    let storage = |name: &str| match &a.s3 {
        Some(ep) => format!(
            "storage: {{ url: s3://tiny-eye/fleet-{run_id}/{name}, s3: {{ endpoint: {ep}, access_key_id: tinyeye, secret_access_key: tinyeye-secret, allow_http: true }} }}"
        ),
        None => format!("storage: {{ url: {}/objects/{name} }}", dir.display()),
    };
    let s3 = a.s3.is_some();

    let names: Vec<String> = (0..a.regions).map(|i| format!("region-{i}")).collect();
    let mut regions = vec![];
    for n in &names {
        regions.push(start(n, n, dir, &storage(n), &ext).await?);
    }
    let baseline = start("baseline", "all", dir, &storage("baseline"), &ext).await?;
    let duck_version: String = baseline.node.with_conn(|c| Ok(c.query_row("SELECT version()", [], |r| r.get(0))?)).await?;

    let mut md = String::new();
    writeln!(md, "# Share-nothing fleet — proof of concept\n")?;
    md.push_str(&shared::run_block());
    writeln!(
        md,
        "Every node keeps its own DuckLake catalog and object prefix and publishes a copy of its catalog; a read-only viewer \
         attaches all copies and queries them as one. Compared against a baseline node that holds every region's data in one \
         catalog. Storage: {}. DuckDB {duck_version}.\n",
        if s3 { "MinIO on the same host (S3 API, ~1 ms per request)" } else { "local disk" },
    )?;

    // ------------------------------------------------------------------ 1. ingest
    let steps = a.hours * 3_600_000 / SCRAPE;
    let now = now_ms() / 60_000 * 60_000;
    let t0 = now - a.hours * 3_600_000;
    let mut total_samples = 0;
    let mut total_series = 0;
    println!("1. ingest: {} regions × {} series × {} h", a.regions, a.series, a.hours);
    let t = Instant::now();
    for r in &regions {
        let sets = region_series(a.series, &r.name);
        total_series += sets.len();
        let (s, _, _) = backfill(&r.base, &sets, t0, 0..steps, 4, 4);
        backfill(&baseline.base, &sets, t0, 0..steps, 4, 4);
        total_samples += s;
    }
    let mut trace_ids = vec![];
    let hour_start = (now - 3_600_000) / 3_600_000 * 3_600_000;
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
    for (i, r) in regions.iter().enumerate() {
        let mut rng = StdRng::seed_from_u64(i as u64 + 1);
        for _ in 0..(a.traces / 500).max(1) {
            let (req, ids) = traces_request(&mut rng, 500, hour_start * 1_000_000, (hour_start + 3_600_000) * 1_000_000);
            trace_ids.extend(ids);
            let body = req.encode_to_vec();
            for base in [&r.base, &baseline.base] {
                let resp = agent.post(&format!("{base}/v1/traces")).header("Content-Type", "application/x-protobuf").send(&body[..])?;
                assert_eq!(resp.status().as_u16(), 200);
            }
        }
    }
    for r in regions.iter().chain([&baseline]) {
        r.node.flush_and_checkpoint().await?;
        r.node.compact(now + 2 * 3_600_000).await?;
    }
    println!("   {total_samples} samples, {} traces per copy in {:.1} s", trace_ids.len(), t.elapsed().as_secs_f64());
    writeln!(
        md,
        "## Data\n\n{} regions, each {} series ({} counters, {} gauges, 120 histogram buckets) over {} h at 15 s, and {} traces × 6 spans: \
         {total_samples} samples and {} spans in total. Every region's series carry `cluster=<region>`. The baseline node received \
         the same requests. All nodes flushed, checkpointed and compacted (one L1 file per table and hour).\n",
        a.regions,
        total_series / a.regions,
        a.series,
        a.series / 4,
        a.hours,
        trace_ids.len() / a.regions,
        trace_ids.len() * 6
    )?;

    // ------------------------------------------------------------------ 2. publish
    println!("2. publish snapshots");
    writeln!(
        md,
        "## Publish (each node, after compaction)\n\n| Node | Copy ms | Gzip ms | PUT ms (both) | Snapshot bytes | Gzipped bytes |\n\
         | :--- | ---: | ---: | ---: | ---: | ---: |"
    )?;
    let scratch = dir.join("scratch");
    std::fs::create_dir_all(&scratch)?;
    for r in &regions {
        let p = publish(r, &scratch).await?;
        println!(
            "   {}: copy {:.0} ms, gzip {:.0} ms, put {:.0} ms, {} → {} bytes",
            r.name, p.copy_ms, p.gzip_ms, p.put_ms, p.bytes, p.gz_bytes
        );
        writeln!(md, "| {} | {:.0} | {:.0} | {:.0} | {} | {} |", r.name, p.copy_ms, p.gzip_ms, p.put_ms, p.bytes, p.gz_bytes)?;
    }

    // ------------------------------------------------------------------ 3 + 4. viewer
    let (from, to) = (t0 + 600_000, now);
    let mut docs: Vec<(&str, String)> = panels(from, to, "60s").into_iter().map(|(n, d)| (n, d.to_string())).collect();
    let present = hex(&trace_ids[trace_ids.len() / 2]);
    docs.push(("trace by id, present", serde_json::json!({"tqs": "3", "id": "t", "kind": "trace", "trace_id": present}).to_string()));
    docs.push((
        "trace by id, absent",
        serde_json::json!({"tqs": "3", "id": "t", "kind": "trace", "trace_id": "0123456789abcdef0123456789abcdef"}).to_string(),
    ));

    println!("3. baseline");
    let mut base_m = vec![];
    for (_, j) in &docs {
        base_m.push(measure_node(&baseline, j.clone(), s3).await?);
    }

    // (round trip ms, mode, first download, first attach, refresh download, refresh attach, per-query results)
    type Run = (u64, Mode, f64, f64, f64, f64, Vec<Measure>);
    let mut runs: Vec<Run> = vec![];
    let mut fresh_viewer = None;
    let rtts: Vec<u64> = if s3 { a.rtt.clone() } else { vec![0] };
    for &rtt in &rtts {
        let endpoint = match (&a.s3, rtt) {
            (Some(ep), 0) => Some(ep.clone()),
            (Some(ep), _) => {
                Some(delay_proxy(ep.trim_start_matches("http://").to_string(), std::time::Duration::from_micros(rtt * 500)).await?)
            }
            (None, _) => None,
        };
        let viewer_storage = match &endpoint {
            Some(ep) => format!(
                "storage: {{ url: s3://tiny-eye/fleet-{run_id}, s3: {{ endpoint: {ep}, access_key_id: tinyeye, secret_access_key: tinyeye-secret, allow_http: true }} }}"
            ),
            None => format!("storage: {{ url: {}/objects }}", dir.display()),
        };
        let secret = endpoint.as_ref().map(|ep| duck::S3Secret {
            endpoint: ep.clone(),
            region: "us-east-1".into(),
            key_id: "tinyeye".into(),
            secret: "tinyeye-secret".into(),
            use_ssl: false,
        });
        for mode in [Mode::Remote, Mode::Download] {
            println!("4. viewer, {}, round trip {rtt} ms", mode.label());
            let mut v = Viewer::open(&ext, &viewer_storage, secret.as_ref(), mode, dir.join(format!("viewer-{rtt}-{}", mode as u8)))?;
            let (dl, att) = v.refresh(&regions).await?;
            let (dl2, att2) = v.refresh(&regions).await?; // what the viewer does every refresh interval
            println!("   first: download {dl:.0} + attach {att:.0} ms; refresh: {dl2:.0} + {att2:.0} ms");
            let mut ms = vec![];
            for (i, (name, json)) in docs.iter().enumerate() {
                let m = tokio::task::block_in_place(|| measure(&v.conn, json, s3))?;
                let b = &base_m[i];
                println!(
                    "   {name:<45} rows {:>5} equal {}  base p50 {:>6.1}  viewer first {:>7.1} p50 {:>7.1}  GETs {} / {}",
                    m.rows.len(),
                    equal(&m.rows, &b.rows),
                    b.p50,
                    m.first_ms,
                    m.p50,
                    b.gets,
                    m.gets
                );
                ms.push(m);
            }
            runs.push((rtt, mode, dl, att, dl2, att2, ms));
            if rtt == 0 && mode == Mode::Download {
                fresh_viewer = Some(v);
            }
        }
    }

    writeln!(
        md,
        "\n## Viewer: attach and refresh ({} snapshots)\n\n\
         The viewer reaches object storage through a TCP proxy that delays each direction by half the round trip (chunks stay pipelined, \
         bandwidth is not limited): a laptop that far from the buckets.\n\n\
         | Round trip | Mode | First download ms | First attach + views ms | Refresh download ms | Refresh attach + swap ms |\n\
         | ---: | :--- | ---: | ---: | ---: | ---: |",
        a.regions
    )?;
    for (rtt, mode, dl, att, dl2, att2, _) in &runs {
        let d = |x: f64| if *mode == Mode::Download { format!("{x:.0}") } else { "—".into() };
        writeln!(md, "| {rtt} ms | {} | {} | {att:.0} | {} | {att2:.0} |", mode.label(), d(*dl), d(*dl2))?;
    }

    let all_equal = runs.iter().all(|r| r.6.iter().zip(&base_m).all(|(m, b)| equal(&m.rows, &b.rows)));
    write!(
        md,
        "\n## Queries: viewer against the baseline node\n\n\
         Every query ran on the baseline node (one catalog with every region's data, next to the bucket) and on the viewer \
         (UNION ALL over {} attached catalogs). \"First\" runs with DuckDB's caches empty (counting the previous query's GETs switches them off, which empties them), so it is what a panel costs the first time; p50 is over 10 warm runs. \
         Results equal to the baseline on every viewer configuration: **{}** (relative tolerance 1e-9; trace rows compared without \
         the `region` column, which the viewer reports per source region and the baseline as `all`).\n\n\
         | Query | Rows | GETs, caches off: baseline / viewer | Baseline p50 ms |",
        a.regions,
        if shared::check(all_equal) { "yes" } else { "NO" }
    )?;
    for (rtt, mode, ..) in &runs {
        write!(md, " {rtt} ms, {}: first / p50 |", if *mode == Mode::Remote { "remote" } else { "downloaded" })?;
    }
    writeln!(md, "\n| :--- | ---: | ---: | ---: |{}", " ---: |".repeat(runs.len()))?;
    for (i, (name, _)) in docs.iter().enumerate() {
        let b = &base_m[i];
        write!(
            md,
            "| `{name}` | {} | {} | {:.1} |",
            b.rows.len(),
            if s3 { format!("{} / {}", b.gets, runs[0].6[i].gets) } else { "—".into() },
            b.p50
        )?;
        for r in &runs {
            let m = &r.6[i];
            write!(md, " {:.0} / {:.0}{} |", m.first_ms, m.p50, if equal(&m.rows, &b.rows) { "" } else { " **≠**" })?;
        }
        writeln!(md)?;
    }

    // ------------------------------------------------------------------ 5. freshness
    println!("5. freshness");
    let mut v = fresh_viewer.context("no local viewer")?;
    let r0 = &regions[0];
    let marker_t = now_ms() / 1000 * 1000;
    let marker = server::receivers::WriteRequest {
        timeseries: vec![server::receivers::TimeSeries {
            labels: vec![label("__name__", "fleet_marker"), label("cluster", &r0.name)],
            samples: vec![server::receivers::Sample { value: 1.0, timestamp: marker_t }],
        }],
    };
    let resp = agent
        .post(&format!("{}/api/v1/write", r0.base))
        .header("Content-Encoding", "snappy")
        .send(&server::receivers::encode_remote_write(&marker)[..])?;
    assert_eq!(resp.status().as_u16(), 204);
    let mq = serde_json::json!({"tqs": "3", "id": "m", "kind": "table", "time": {"from": marker_t - 60_000, "to": marker_t + 1000},
        "queries": [{"name": "m", "metric": "fleet_marker", "fn": {"op": "last"}, "agg": {"op": "sum"}}]})
    .to_string();
    let before = tokio::task::block_in_place(|| run_doc(&v.conn, &mq))?.len();
    let t = Instant::now();
    r0.node.flush_now().await?;
    let flush_ms = ms_since(t);
    let t = Instant::now();
    r0.node.checkpoint().await?;
    let ckpt_ms = ms_since(t);
    let p = publish(r0, &scratch).await?;
    let (dl_ms, att_ms) = v.refresh(&regions).await?;
    let after = tokio::task::block_in_place(|| run_doc(&v.conn, &mq))?;
    let seen = after.iter().any(|r| (r.3 - 1.0).abs() < 1e-9);
    let work = flush_ms + ckpt_ms + p.copy_ms + p.gzip_ms + p.put_ms + dl_ms + att_ms;
    println!("   visible before: {before} rows, after: {seen}; work on the path {work:.0} ms");
    writeln!(
        md,
        "\n## Freshness: one new sample from region-0 to the viewer (no added round trip)\n\n\
         | Step | ms |\n| :--- | ---: |\n| Flush (Parquet + PUT + envelope) | {flush_ms:.0} |\n| Checkpoint (register in the node's catalog) | {ckpt_ms:.0} |\n\
         | Publish: copy catalog + gzip | {:.0} |\n| Publish: PUT | {:.0} |\n\
         | Viewer refresh: download {n} snapshots | {dl_ms:.0} |\n| Viewer refresh: attach + swap views | {att_ms:.0} |\n\
         | **Work on the path** | **{work:.0}** |\n\nVisible on the viewer before: {}; after: {}. The lag a user sees is set by the \
         intervals, not this work: at most `flush_interval` + `checkpoint_interval` + publish interval + viewer refresh interval.\n",
        p.copy_ms + p.gzip_ms,
        p.put_ms,
        if shared::check(before == 0) { "no" } else { "yes (unexpected)" },
        if shared::check(seen) { "yes" } else { "**no**" },
        n = regions.len()
    )?;

    // ------------------------------------------------------------------ 6. catalog growth
    println!("6. catalog growth: {} small flushes on region-0", a.growth);
    writeln!(
        md,
        "\n## Catalog growth on one node ({} flush + checkpoint cycles, each one scrape of every series)\n\n\
         | After | Files in catalog | DuckLake snapshots | Snapshot bytes | Gzipped bytes | Copy + gzip ms | PUT ms | Viewer refresh ms |\n\
         | :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
        a.growth
    )?;
    let sets = region_series(a.series, &r0.name);
    let counts = |node: Arc<Node>| async move {
        node.with_conn(|c| {
            let mut files = 0i64;
            for t in Table::ALL {
                files +=
                    c.query_row(&format!("SELECT count(*) FROM ducklake_list_files('lake', '{}')", t.name()), [], |r| r.get::<_, i64>(0))?;
            }
            let snaps: i64 = c.query_row("SELECT count(*) FROM ducklake_snapshots('lake')", [], |r| r.get(0))?;
            Ok((files, snaps))
        })
        .await
    };
    let mut growth_row = |label: String, p: Published, files: i64, snaps: i64, view: f64| -> Result<()> {
        println!("   {label}: {files} files, {snaps} snapshots, {} → {} bytes, refresh {view:.0} ms", p.bytes, p.gz_bytes);
        writeln!(
            md,
            "| {label} | {files} | {snaps} | {} | {} | {:.0} | {:.0} | {view:.0} |",
            p.bytes,
            p.gz_bytes,
            p.copy_ms + p.gzip_ms,
            p.put_ms
        )?;
        Ok(())
    };
    for k in 0..=a.growth {
        if k % (a.growth / 4).max(1) == 0 {
            let (f, s) = counts(r0.node.clone()).await?;
            let p = publish(r0, &scratch).await?;
            let (d, at) = v.refresh(&regions).await?;
            growth_row(format!("{k} flushes"), p, f, s, d + at)?;
        }
        if k < a.growth {
            // One scrape of every series (a repeat of a backfilled step: compaction deduplicates it).
            backfill(&r0.base, &sets, t0, k as i64..k as i64 + 1, 1, 1);
            r0.node.flush_and_checkpoint().await?;
        }
    }
    r0.node.compact(now_ms() + 2 * 3_600_000).await?;
    r0.node
        .with_conn(|c| {
            c.execute_batch(
                "CALL ducklake_expire_snapshots('lake', older_than => now()); CALL ducklake_cleanup_old_files('lake', cleanup_all => true);",
            )?;
            Ok(())
        })
        .await?;
    let (f, s) = counts(r0.node.clone()).await?;
    let p = publish(r0, &scratch).await?;
    let (d, at) = v.refresh(&regions).await?;
    growth_row("then compaction + snapshot expiry".into(), p, f, s, d + at)?;

    writeln!(
        md,
        "\nReproduce: `make fleet-poc` (MinIO) or `cargo run --release -p server --bin tiny-eye-fleet` (local disk). \
         One run on one machine, under whatever else the machine was doing; compare the viewer with the baseline of the same run.\n"
    )?;

    if let Some(out) = a.out {
        let p = if Path::new(&out).is_absolute() { PathBuf::from(out) } else { root.join(out) };
        std::fs::create_dir_all(p.parent().context("report path")?)?;
        std::fs::write(&p, &md)?;
        println!("report: {}", p.display());
    } else {
        println!("{md}");
    }
    shared::finish()?;
    Ok(())
}
