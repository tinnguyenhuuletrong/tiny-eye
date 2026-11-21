# Blob-First Observability Engine — Implementation Concept v1 (Rust)

**Status:** ready to start implementation. Supersedes the v5 specification and its review
([docs/v5-review.md](docs/v5-review.md)) wherever they disagree.
**Evidence:** every design claim below is tagged with how it is known:

| Tag | Meaning |
| :--- | :--- |
| **[E1]…[E9]** | Proven by this repo's PoC (`scripts/run-poc.sh`; numbers in [results/poc-results.md](results/poc-results.md)) |
| **[P]** | Observed in a one-off probe during the PoC (DuckDB 2.0.0 + DuckLake, same stack), not yet a scripted experiment |
| **[U]** | Unverified: a design choice or a known-elsewhere fact that the PoC did not exercise |

PoC stack: Rust (`parquet` 60, `arrow` 60, `object_store` 0.14, `duckdb` 1.20000.1 = DuckDB 2.0.0),
DuckLake extension 2.0.0 with a PostgreSQL 16 catalog, MinIO as S3. Scale was small (≈ 200k spans,
144k counter samples, 2 regions, 4 ingest "nodes" in one process, local MinIO). **The PoC proves
mechanisms and correctness, not performance at production scale.**

---

## 1. What we are building

A single Rust binary that ingests OpenTelemetry, Prometheus remote-write and Loki push, stores
everything as sorted ZSTD Parquet in object storage, keeps a DuckLake catalog of those files, and
answers queries with embedded DuckDB, for people (Grafana) and for AI agents (MCP). Compute nodes
hold no durable state; object storage plus the catalog are the database.

### 1.1 v1.0 scope

In: metrics, traces, logs; OTLP gRPC/HTTP, Prometheus `remote_write`, Loki push; Prometheus query
API (via §10.1), Tempo and Loki read-API subsets; TQS v2.1; MCP (read tools + one admin tool);
materialized panels; Prometheus rules + Alertmanager; single node and cluster; per-region buckets.

Out of v1.0: RUM and sourcemaps, Prometheus scrape mode (users keep Prometheus agent, the OTel
Collector or Alloy as the scraper), historical TSDB/Loki importers, the Grafana dashboard migrator,
an embedded web UI, multi-tenant billing/isolation beyond a `tenant` column.

---

## 2. Architecture

```text
 OTLP / remote_write / Loki push                          Grafana · Prometheus front (§10.1) · MCP · laptop
              │                                                               │
              ▼                                                               ▼
 ┌──────────────────────────────┐   hot-tail RPC (watermarked)   ┌──────────────────────────────┐
 │ INGEST role (N, stateless)   │◄───────────────────────────────│ QUERY role (N, stateless)    │
 │ decode → arena → durability  │                                │ plan → prune → DuckDB        │
 │ → sort → Parquet → PUT       │                                │ + hot tails + caches         │
 │ → envelope (If-None-Match)   │                                └──────────────┬───────────────┘
 └──────────────┬───────────────┘                                               │ read
                │ PUT data + envelopes                                          │
                ▼                                                               ▼
 ┌─────────────────────────────────────────────────────────────────────────────────────────────┐
 │ OBJECT STORAGE, one bucket per region: telemetry-<region>/{spans,logs,metrics_*,commits}/…  │
 └──────────────▲──────────────────────────────────────────────────────────────▲───────────────┘
                │ list/get envelopes, write L1, delete                         │ snapshot file (laptop)
 ┌──────────────┴───────────────────────────────────────────────┐              │
 │ COORDINATOR role (1 per region, leased, fenced)              │──────────────┘
 │ checkpointer · compactor · retention · materializer ·        │
 │ catalog-snapshot exporter · rule evaluation hook             │
 └──────────────┬───────────────────────────────────────────────┘
                │ one transaction per tick
                ▼
 ┌──────────────────────────────────────────────┐
 │ DuckLake catalog in PostgreSQL               │  (single-node mode: DuckLake on a DuckDB file,
 │ files + stats + snapshots + inlined partials │   synced to S3 with If-Match)
 └──────────────────────────────────────────────┘
```

One binary, roles chosen by flag: `--role=all` (single node), `ingest`, `query`, `coordinator`, and
`mcp --read-only` (laptop).

---

## 3. Decisions

| # | Decision | Evidence |
| :--- | :--- | :--- |
| D1 | **Rust.** Ingest path uses `arrow`/`parquet`/`object_store` only — no DuckDB on the write path. DuckDB (bundled via `duckdb` crate) is used by query, coordinator and laptop roles. | [E2] wrote 44 files + 11 envelopes with no DuckDB. |
| D2 | **No ingest-side state for counters; no routing.** Raw points store `value` only. Increase is computed at read time over L0 (`value - lag(value)`) and materialized as `positive_delta` by the compactor in L1. | [E5]: 0 series differ from the best possible result on both paths; the v5 ingest-state rule lost 9.33 % of the total under the same crash. |
| D3 | **DuckLake is the catalog.** Files are written by us and registered with `ducklake_add_data_files`; DuckLake owns their lifecycle from then on. | [E3] registration from two buckets, atomic, idempotent; [E6] DuckLake deleted all 11 external L0 files after expiry. |
| D4 | **Our own compactor**, with our Parquet writer (sort + bloom), swapping files with `DELETE … WHERE filename IN (…)` + `add_data_files` in one transaction. DuckLake's `merge_adjacent_files` is not used for spans. | [E6] 178,867 rows before and after; [P] DuckDB's writer emits no `trace_id` bloom filter. |
| D5 | **Views (materialized panels) are fed from our ingest envelopes, never from the DuckLake change feed.** | [E6] the change feed reported 178,867 inserts + 178,867 deletes for a compaction with 0 net new rows. |
| D6 | **Percentiles are log-bucket histograms** (scale 5, bucket width 2^(1/32)) merged with `SUM` in SQL. No sketch BLOBs. | [E8] worst error 1.01 % on p50/p95/p99; [P] DuckDB's t-digest state cannot be stored in Parquet and merged later. |
| D7 | **Commit queue + coordinator lease use conditional writes** on object storage; every catalog write is additionally fenced (§6.3). | [E1] MinIO: second create refused, stale ETag refused, 1 of 8 racers won. |
| D8 | **Per-node watermark for hot tails**, read in the same catalog transaction as the scan. | [E7] correct in all three states; without it, double counts 60,000 rows. |
| D9 | **One catalog, one bucket per region**, files referenced by absolute path. | [E3] 2 regions, 2 buckets, one catalog; [E4] cross-region trace stitched. |
| D10 | **Laptop mode reads a catalog snapshot file from S3**, produced by flushing inlined rows and copying DuckLake's metadata tables. | [E9] attach 44 ms, trace lookup 28 ms, writes refused; raw copy without the flush fails. |
| D11 | **PromQL in two steps:** M1 serves remote_read to an unmodified Prometheus as "PromQL front"; native Rust evaluation of hot shapes later (§10.1). | [U] |
| D12 | **Ship the DuckDB extensions with the binary**, pinned to the bundled DuckDB version; never install at run time. | [P] the public extension host was unreachable from the PoC machine; extensions came from PyPI wheels. |
| D13 | **Apache-2.0**; no AGPL code (Loki, Tempo, Mimir, Grafana server). Prometheus (Apache-2.0) as an external process is fine. | [U] |

---

## 4. Storage layout

### 4.1 Object keys

```
s3://telemetry-<region>/
  commits/<region>/<ulid>-<node_id>.json                 # envelopes; deleted after the catalog commit
  catalog/<tenant>/coordinator.lease                     # conditional-PUT lease (single-node + cluster)
  catalog/<tenant>/snapshot-latest.ducklake              # laptop/agent read-only catalog
  spans/tenant=<t>/date=<d>/hour=<h>/l0-<ulid>-<node>.parquet
  spans/tenant=<t>/date=<d>/hour=<h>/l1-<ulid>-compacted.parquet
  logs/…  metrics_points/…  metrics_series/…  metrics_hist/…   (same pattern)
```

The PoC used `window=<n>` in place of `date/hour`; the hierarchy only serves humans, lifecycle
backstops and residency audits. DuckLake does not read it (it stores paths and stats) [E3].

### 4.2 Parquet writer settings (Rust `parquet` crate)

| Setting | Value | Evidence |
| :--- | :--- | :--- |
| Compression | ZSTD level 3 | [E2] |
| Statistics | page-level (`EnabledStatistics::Page`), page index written | [E2] |
| Row group | 122,880 rows in production (PoC used 4,096–8,192 to get several groups per small file) | [E4] |
| `trace_id` | dictionary off, SBBF bloom filter, FPP 0.01 | [E4], [P] bloom present on all 13 L1 row groups |
| `timestamp_ms` | `DELTA_BINARY_PACKED` | [E2] |
| `value` (DOUBLE) | `BYTE_STREAM_SPLIT` | [E2] |

### 4.3 Tables (DuckLake DDL, types as the Rust writer emits them)

Column types must match what the writer produces, because `add_data_files` maps by name and
checks types [E3]. Proven mapping: Arrow `FixedSizeBinary(16)` → `BLOB`; `UInt64` → `UBIGINT`;
`UInt8` → `UTINYINT`; `Timestamp(ns)` → `TIMESTAMP_NS`; Arrow `Map<Utf8,Utf8>` →
`MAP(VARCHAR, VARCHAR)`.

```sql
CREATE TABLE spans (
  tenant VARCHAR, region VARCHAR, trace_id BLOB, span_id UBIGINT, parent_span_id UBIGINT,
  start_time_ns TIMESTAMP_NS, duration_ns UBIGINT, service_name VARCHAR, span_name VARCHAR,
  span_kind UTINYINT, status_code UTINYINT, status_message VARCHAR, http_status_code USMALLINT,
  resource_attrs MAP(VARCHAR, VARCHAR), span_attrs MAP(VARCHAR, VARCHAR),
  events STRUCT(ts TIMESTAMP_NS, name VARCHAR, attrs MAP(VARCHAR, VARCHAR))[],
  links  STRUCT(trace_id BLOB, span_id UBIGINT, attrs MAP(VARCHAR, VARCHAR))[]);
-- sort: (trace_id, start_time_ns)

CREATE TABLE traces_index (                       -- one row per (trace, file): commutative partial segments
  tenant VARCHAR, region VARCHAR, trace_id BLOB, has_root BOOLEAN,
  root_service_name VARCHAR, root_span_name VARCHAR, root_status_code UTINYINT,
  min_start_ns TIMESTAMP_NS, max_end_ns TIMESTAMP_NS,
  span_count UINTEGER, error_count UINTEGER, services VARCHAR[]);
-- sort: (min_start_ns, trace_id)

CREATE TABLE metrics_series (                     -- one row per (series_id, day) seen
  tenant VARCHAR, region VARCHAR, day DATE, series_id UBIGINT, logical_series_id UBIGINT,
  metric_name VARCHAR, metric_type UTINYINT, temporality UTINYINT, unit VARCHAR,
  labels MAP(VARCHAR, VARCHAR));

CREATE TABLE metrics_points (
  tenant VARCHAR, region VARCHAR, metric_name VARCHAR, series_id UBIGINT,
  timestamp_ms BIGINT, value DOUBLE, is_stale BOOLEAN,
  positive_delta DOUBLE);                         -- NULL in L0; set by the compactor in L1
-- sort: (series_id, timestamp_ms)

CREATE TABLE metrics_hist (                       -- OTel exponential / Prometheus native histograms
  tenant VARCHAR, region VARCHAR, metric_name VARCHAR, series_id UBIGINT, timestamp_ms BIGINT,
  scale TINYINT, zero_threshold DOUBLE, zero_count UBIGINT,
  positive MAP(SMALLINT, UBIGINT), negative MAP(SMALLINT, UBIGINT),
  count UBIGINT, sum DOUBLE, min DOUBLE, max DOUBLE, reset_hint UTINYINT);

CREATE TABLE logs (
  tenant VARCHAR, region VARCHAR, timestamp_ns TIMESTAMP_NS, service_name VARCHAR,
  severity_number UTINYINT, severity_text VARCHAR, pattern_hash UBIGINT, pattern_version UTINYINT,
  body VARCHAR, trace_id BLOB, span_id UBIGINT,
  resource_attrs MAP(VARCHAR, VARCHAR), log_attrs MAP(VARCHAR, VARCHAR));
-- sort: (service_name, severity_number, timestamp_ns)

CREATE TABLE applied_envelopes (                  -- idempotency + hot-tail watermarks
  envelope_id VARCHAR, region VARCHAR, node_id VARCHAR, node_epoch BIGINT, seq_to BIGINT, files INTEGER);

CREATE TABLE view_partials (                      -- materialized panels, long format, inlined
  tenant VARCHAR, panel_id VARCHAR, panel_version INTEGER, bucket_ts BIGINT, region VARCHAR,
  group_key VARCHAR, agg_name VARCHAR, n UBIGINT, s DOUBLE, mn DOUBLE, mx DOUBLE,
  last_ts BIGINT, last_val DOUBLE, hist_bucket SMALLINT);
```

Notes:
- `trace_id` is `BLOB` (Arrow `FixedSizeBinary(16)`), looked up with `from_hex(...)` [E4]. The
  `UUID` logical type would give nicer display; switch only after a test that `add_data_files`
  and bloom filters still work with it [U].
- Only `spans` columns that the PoC wrote are proven through `add_data_files`: everything up to
  `span_attrs`. `events`/`links` (lists of structs) are [U] — test them in the first sprint.
- In the PoC, `view_partials` histograms used a long format (one row per bucket), which inlines
  well [E8]. `MAP` columns inside inlined rows are stored as `varchar` by the PostgreSQL backend
  [P], so keep inlined tables to scalar columns.

### 4.4 Series identity

```
series_id = xxh64( for part in [tenant, region, metric_name, canonical_labels]:
                     u32_le(len(part)) ‖ part )
canonical_labels = labels sorted by key, each as u32_le(len(k)) ‖ k ‖ u32_le(len(v)) ‖ v,
                   excluding __name__
logical_series_id = same, excluding the configured HA replica label
```

Length-prefixing makes the encoding injective [E2 uses it]. Region and tenant come from the
receiving instance's config, never the payload. On a known id with different labels: count
`series_id_collisions_total` and log both sets.

---

## 5. Write path (ingest role)

### 5.1 Steps

1. **Decode** OTLP (`opentelemetry-proto` + `tonic`/`prost`), remote_write (`prost` + `snap`), Loki
   push (protobuf + JSON). Reject samples older than `ingest.max_age` (1 h) or further ahead than
   `ingest.max_future` (10 min).
2. **Admit into the arena**, a byte-accounted buffer per signal. Over 75 %: spill frozen batches to
   ephemeral disk as Arrow IPC. Over 95 %: `429` / `RESOURCE_EXHAUSTED`.
3. **Persist per durability mode** (§5.2), then acknowledge.
4. **Flush** when a buffer reaches 64 MB uncompressed or 60 s: sort, write Parquet in memory, `PUT`
   each file [E2].
5. **Commit** one envelope per flush with `If-None-Match: *` [E1, E2]. Files are invisible until the
   checkpointer applies it.
6. **Retain** the flushed batch in RAM until the catalog shows it committed plus a grace period
   (2 × checkpoint interval) — it serves the hot tail (§8.3).

No peer forwarding and no consistent hashing: a load balancer may spray requests [E2, E5].

### 5.2 Durability modes (acknowledgement contract)

| Mode | Ack after | Lost on one node crash | Status |
| :--- | :--- | :--- | :--- |
| `memory` | arena admission | that node's unflushed batch; cumulative counter **totals** survive | [E2, E5]: 21,133 spans and 12,063 samples lost; counter totals exact except 15 series with a reset inside the lost batch |
| `wal` | group-committed `fsync` (100–250 ms) to local disk; replayed on restart | nothing while the disk survives | [U] |
| `replicated` | batch held by 2 in-region peers | needs 2 failures in one flush interval | [U] |
| `s3-wal` | ≤ 1 s group-commit segment PUT to object storage | nothing | [U] |

Default proposal: `wal` for single node, `replicated` for cluster. The team decides the default
RPO (§16).

### 5.3 Envelope (format 1, as in [E2/E3])

```json
{ "format": 1, "envelope_id": "<ulid>", "kind": "ingest", "tenant": "default",
  "region": "us-east", "node_id": "us-east-n0", "node_epoch": 1, "seq_to": 4,
  "files": [ { "table": "spans", "path": "s3://telemetry-us-east/spans/…/l0-…parquet",
               "bytes": 694512, "rows": 16743 } ] }
```

`kind ∈ {ingest, compaction, retention, erasure}`; v1 uses only `ingest` envelopes from nodes —
coordinator-side changes are direct catalog transactions (§7). `node_epoch` changes on every
process start; `seq_to` restarts at 0 per epoch [E2 restart].

---

## 6. Commit path (coordinator: checkpointer)

### 6.1 Algorithm (proven per envelope in [E3])

```text
every 5 s, holding the lease:
  for region in regions: list commits/<region>/ ; parse envelopes ; sort by ulid
  BEGIN
    for env in pending:
      if exists applied_envelopes.envelope_id = env.id: mark done; continue    # crash replay
      for f in env.files: CALL ducklake_add_data_files('lake', f.table, f.path)
      INSERT INTO applied_envelopes VALUES (env.id, env.region, env.node_id, env.node_epoch, env.seq_to, len(files))
  COMMIT                                                                        # one snapshot per tick
  delete applied envelope objects
```

Proven: a missing file aborts the whole envelope (files 11 → 11, no marker row); a crash between
`COMMIT` and the envelope delete is harmless (skipped 1, no duplicate rows) [E3]. Commit latency with
4 files per envelope: p50 41.9 ms, max 46.7 ms [E3].

**Change from the PoC:** apply all pending envelopes in **one** transaction per tick, so the catalog
grows by one snapshot per 5 s instead of one per envelope [U — measure in M1].

### 6.2 Concurrency

Only the coordinator writes the catalog, so checkpointer, compactor, retention and materializer
never race each other. Concurrent writers do work: 4 writers × 10 inserts each into one table gave 0
errors, p50 16 ms, max 972 ms [P]. That worst case is the reason for a single writer. A compactor
swap racing an `add_data_files` on the same table was **not** tested [U].

### 6.3 Lease and fencing

- Lease object `catalog/<tenant>/coordinator.lease`: create with `If-None-Match: *`, renew with
  `If-Match: <etag>` every 10 s, TTL 30 s [E1].
- Fencing: the lease body carries `coordinator_epoch`. A catalog table
  `coordinator_fence(epoch BIGINT)` is updated in every catalog transaction with
  `UPDATE … SET epoch = :mine WHERE epoch <= :mine`; zero rows updated → abort, step down [U].
- In single-node zero-dependency mode, the DuckDB-file catalog is uploaded after each tick with
  `If-Match`; envelopes are deleted only after the upload that contains them succeeds [U].

---

## 7. Compaction, retention, ownership (coordinator)

### 7.1 Compaction

Per `(tenant, region, table, hour)`, after the hour closes plus 10 min grace:

- **Spans/logs:** query the L0 rows sorted (`ORDER BY trace_id, start_time_ns`) as Arrow from DuckDB,
  write L1 with the Rust writer (bloom filters), `PUT`, then in one transaction
  `DELETE FROM spans WHERE filename IN (<L0…>)` + `ducklake_add_data_files(<L1>)` [E6].
- **Metrics:** `COPY (SELECT …, <positive_delta window expression> … ORDER BY series_id, timestamp_ms)
  TO 's3://…/l1-….parquet'` with DuckDB, then the same swap [E5]. Bloom on `series_id` comes free
  from DuckDB's writer because the column is dictionary-encoded [P].
- The previous hour's last point per series seeds the window (cross-hour boundary) [U — the PoC
  compacted one partition with no prior hour].
- Effect on lookup: 11 L0 files → 2 L1 files took a trace lookup from 26 GETs / 294.7 KiB to 6 GETs
  / 155.6 KiB [E6].

### 7.2 Retention and erasure

`DELETE FROM <table> WHERE filename IN (<expired files>)` (whole files, so no delete files are
written [P]), then `ducklake_expire_snapshots(older_than => now() - snapshot_retention)` and
`ducklake_cleanup_old_files(...)`. DuckLake deletes the objects, including ones we wrote outside its
`DATA_PATH` and in other buckets [E6]. S3 lifecycle rules stay as a backstop set to retention +
7 days.

Consequences of DuckLake owning file deletion:
- The coordinator needs delete permission in every regional bucket.
- **Never register one object in two tables or two catalogs**: the first cleanup deletes it under
  the other.
- Snapshot expiry also bounds catalog growth (one snapshot per tick); run it every hour, keeping
  snapshots for `max(longest query timeout, hot-tail grace) + margin`.

Erasure (GDPR) = rewrite the affected files without the rows, then the same swap [U].

---

## 8. Read path (query role)

### 8.1 Query execution

- One `duckdb::Connection` per worker thread (cloned from one database handle); queries run on
  `tokio::task::spawn_blocking` [U].
- DuckDB settings per query: `memory_limit`, `threads`, timeout; enable `enable_external_file_cache`
  and `parquet_metadata_cache` in production (the PoC disabled them to count cold GETs) [P: the
  settings exist in 2.0.0].
- Every query pins one catalog snapshot by running inside one read transaction [E7].

### 8.2 Trace lookup — what it costs

DuckLake prunes files by column min/max. Random trace ids make every file's range nearly the whole
id space, so **no file is pruned by the catalog**. Each file then costs a footer GET and a bloom GET
[E4, absent id: 22 GETs for 11 files].

| Layout (11 L0 files) | GETs | KiB read | Time |
| :--- | ---: | ---: | ---: |
| sorted by `trace_id` + bloom (concept) | 26 | 294.7 | 22.4 ms |
| sorted by `trace_id`, no bloom | 22 | 623.8 | 36.6 ms |
| v5: sorted by service first + bloom | 38 | 509.6 | 39.7 ms |
| concept layout after compaction (2 L1 files) | 6 | 155.6 | 14.7 ms |

So the cost of a trace lookup is linear in the number of files in the time range. Rules that
follow:
1. A trace lookup always carries a time range. It comes from `traces_index`, the exemplar, the log
   line or the user; the default is ±1 h around "now".
2. Compact hourly (§7.1). One hour with 4 nodes flushing every minute is 240 L0 files, so 480+ GETs
   before compaction and about 2 per region after.
3. If lookups over long ranges matter, add a trace-id → file index (an extra table of
   `(file, trace_id_prefix)` blooms) [U, not v1].

Cross-region stitching needs nothing special: one table, files from both buckets; the lookup
returned 5/5 spans from 2 regions across a flush boundary [E4].

### 8.3 Hot tail (unflushed and not-yet-committed data)

Protocol, proven in-process [E7]:
1. Query opens a read transaction (pins snapshot S) and reads
   `W[node, epoch] = max(seq_to) FROM applied_envelopes` at S.
2. Each live node in scope returns rows from retained batches with `seq > W[node, epoch]`, plus
   `retained_from`.
3. If a node already dropped a batch the snapshot does not include (`retained_from > W`), re-pin and
   retry (≤ 2), then answer with a partial-result warning [U].
4. Batches stay in RAM until committed + grace (§5.1 step 6).

[E7] results: before flush 238,867 (expected 238,867); committed but still retained 238,867; naive
`UNION ALL` without the watermark 298,867 (double count of 60,000); after drop 238,867.

Performance flag: scanning a 60,000-row Arrow batch through the `arrow()` table function took
115.9 ms against 6.7–10.3 ms for the lake count. The vtab copies every column; it has no projection
pushdown [P]. M1 must measure the alternative of appending hot batches into a per-node DuckDB
in-memory table (`appender-arrow`) [U].

Cross-node hot-tail RPC (Arrow Flight or gRPC with Arrow IPC), with a 150 ms budget per region, is
[U].

### 8.4 Counters

```sql
-- L0 (recent, not yet compacted): increase at read time
WITH d AS (SELECT series_id, value,
                  value - lag(value) OVER (PARTITION BY series_id ORDER BY timestamp_ms) AS diff
           FROM metrics_points WHERE …)
SELECT series_id, sum(CASE WHEN diff IS NULL THEN 0 WHEN diff >= 0 THEN diff ELSE value END) …

-- L1 (compacted): sum(positive_delta)
```

Both gave 0 series differing from the best possible result [E5]. A query spanning L0 and L1 uses
`coalesce(positive_delta, <lag expression>)` and seeds the lag with the last L1 point before the
L0 range [U].

Semantics table (first sample = 0; `value < prev` = reset, delta = `value`; OTel `start_time`
change = reset; delta temporality = `value`; staleness NaN ignored; HA replica selection on
`logical_series_id`): [docs/v5-review.md §5.2–5.3](docs/v5-review.md). The PoC proved first sample
and reset [E5]; the rest is [U].

---

## 9. Materialized panels (coordinator: materializer)

- **Input:** rows of files listed in `kind = ingest` envelopes as the checkpointer applies them,
  never `ducklake_table_insertions` (D5, [E6]).
- **State:** `view_partials` (long format) holding additive partials; open buckets live in RAM and
  are written once sealed (2 min).
- **Inlining:** set `data_inlining_row_limit` (PoC: 10,000). Small partial inserts write no Parquet
  file [E8: 1,339 rows, 0 files]. `ducklake_flush_inlined_data` moves them to Parquet when needed
  [E9].
- **Read latency:** a merge + 3 quantiles × 4 services over inlined partials took p50 12.5 ms, max
  15.7 ms [E8]; a 10-row inlined read took ~20 ms [P]. **The v5 target "< 1 ms" is not reachable
  through a PostgreSQL catalog; target p95 < 50 ms.**
- Mergeable operations, finalize rules, alignment and panel versioning: [docs/v5-review.md §5.8](docs/v5-review.md).

---

## 10. Query interfaces

### 10.1 PromQL (Rust-specific plan)

No production-grade PromQL engine exists as a Rust library, and writing one is a project of its
own. Two stages:

1. **M1 — Prometheus as the PromQL front.** Implement Prometheus `remote_read` (protobuf + snappy,
   streamed chunked response) plus `/api/v1/series`, `/labels`, `/label/<n>/values`, `/metadata`.
   Ship a reference deployment where an unmodified Prometheus runs with no scrape targets,
   `remote_read` pointing at us with `read_recent: true`, our rule files and Alertmanager. Grafana
   talks to that Prometheus. Full PromQL semantics, rules and alerts, and zero engine code. Cost: no
   pushdown, so raw samples cross the wire, and it is a second process. Recording rules come back
   through `remote_write` [U].
2. **M4+ — native subset in Rust.** Parse with the `promql-parser` crate. Evaluate selectors,
   `rate`/`increase`/`irate`, `sum|avg|min|max|count by`, `histogram_quantile`, scalar/one-to-one
   binary ops and `offset` with SQL pushdown. Proxy anything else to the front. Gate: the Prometheus
   compliance tester, published per function [U].

### 10.2 TQS v2.1, Tempo, Loki

TQS v2.1 schema (closed formula grammar, filter-key grammar, per-signal validation, limits) is
specified in [docs/v5-review.md §5.9](docs/v5-review.md) and adopted unchanged. Compile to SQL from an
AST; every value is a bound parameter. Tempo (`/api/traces/<id>`, `/api/search`, tag endpoints) and
a Loki LogQL subset are implemented clean-room against their public HTTP APIs (§5.12 of the review).

### 10.3 MCP

`rmcp` crate, stdio + streamable HTTP. Tools: `discover_telemetry_catalog`, `run_telemetry_query`
(TQS inlined in the tool schema), `get_trace_waterfall`, `search_logs`, `correlate_anomalies`;
`save_materialized_dashboard_panel` only with admin scope and never in `--read-only`. Every result
reports `rows_total`, `truncated`, `bytes_scanned`, `snapshot_id`, `freshness_s`, `partial_regions`,
and returns telemetry text in a delimited data field. The trace waterfall query is proven [E4].

---

## 11. Laptop / agent mode

`blobobs mcp --read-only --catalog s3://telemetry-us-east/catalog/default/snapshot-latest.ducklake`

- The coordinator exports the snapshot every N minutes (default 5) [E9]:
  1. `CALL ducklake_flush_inlined_data('lake')`. Required: the PostgreSQL backend stores inlined
     rows in backend-specific types (`bytea` for `VARCHAR`; `varchar` for `UBIGINT`,
     `TIMESTAMP_NS`, `MAP`), so copying them makes an unreadable DuckDB catalog. The first PoC run
     failed exactly this way.
  2. Copy every `ducklake_*` metadata table except `ducklake_inlined_data_*` into a new DuckDB file;
     create `ducklake_inlined_data_tables` empty.
  3. `PUT` with `If-Match` on the previous ETag.
- The laptop attaches it `READ_ONLY`. In the PoC: 44 ms; the trace lookup and the counter totals
  equalled the cluster's; a write was refused [E9].
- Freshness = export interval + flush interval. No hot tail.
- **Risk:** step 2 depends on DuckLake's internal catalog table set. Gate it with a CI test that runs
  E9 on every DuckLake upgrade. The fallback is a read-only PostgreSQL role reached over the network.
- Credentials: a read-only object-storage key per tenant prefix, never the ingest key [U].

---

## 12. Multi-region

- Ingest writes only to its region's bucket; one catalog references all buckets [E3, E4].
- Catalog: PostgreSQL primary in one region with read replicas elsewhere; query roles read metadata
  from the local replica; commits go coordinator → primary [U].
- The PoC used one DuckDB secret for one endpoint. Real regions need one scoped secret per bucket
  (`CREATE SECRET … SCOPE 's3://telemetry-eu-west'`) [U].
- Residency (`in-region-only` regions answer only federated aggregates) and egress-aware planning:
  [docs/v5-review.md §5.11](docs/v5-review.md) [U].

---

## 13. Security

Bearer tokens → `{tenant, scopes}` on every API including MCP; mTLS between nodes; ingest-time
redaction of known secret attributes; query cost limits (memory, threads, timeout, bytes scanned,
concurrency) on every surface; read-only laptop keys. Full list: [docs/v5-review.md §5.15](docs/v5-review.md).
Nothing here was exercised by the PoC [U].

---

## 14. Rust workspace

```
crates/
  core/         envelope, schemas (Arrow + DuckLake DDL from one definition), series identity,
                counter semantics, log buckets, config
  write/        arena, durability (wal/replicated/s3-wal), sorted batch builders, Parquet writer, flusher
  receivers/    otlp (tonic), prom_remote_write (axum + prost + snap), loki_push
  catalog/      DuckLake access: apply envelopes, swap, watermarks, lease + fence, snapshot export
  coordinator/  loops: checkpointer, compactor, retention, materializer, exporter
  query/        planner, DuckDB pool, hot-tail client/server, TQS compiler, caches
  api/          remote_read + Prometheus metadata API, Tempo API, Loki API, TQS HTTP
  mcp/          rmcp server
  bin/          `blobobs` binary, roles
```

Key crates: `arrow`/`parquet` 60, `object_store` 0.14 (`aws`), `duckdb` 1.20000.x (`bundled`),
`tokio`, `axum`, `tonic`, `prost`, `opentelemetry-proto`, `snap`, `xxhash-rust`, `ulid`, `rmcp`,
`promql-parser` (M4), `jsonschema`.

Interfaces to keep stable from day one:

```rust
pub trait Durability: Send + Sync {
    async fn persist(&self, batch: &IngestBatch) -> Result<Persisted>;   // ack only after Ok
}
pub trait Catalog: Send {
    fn apply(&mut self, envelopes: &[Envelope]) -> Result<SnapshotId>;   // one txn, idempotent
    fn swap(&mut self, table: &str, remove: &[ObjectPath], add: &[ObjectPath]) -> Result<SnapshotId>;
    fn watermarks(&self, at: SnapshotId) -> Result<HashMap<(NodeId, Epoch), Seq>>;
}
pub trait HotTail: Send + Sync {
    fn scan(&self, fragment: &ScanFragment, watermark: Seq) -> Result<(Vec<RecordBatch>, Seq)>;
}
```

**Schema single-sourcing:** in the PoC, the Arrow schema (`write.rs`) and the DuckLake DDL
(`lake.rs`) are two hand-kept copies. `add_data_files` fails at commit time on a mismatch [E3], so
generate both from one definition in `core`.

**Packaging:** the container image carries `httpfs`, `ducklake` and `postgres_scanner` extension
files for the exact bundled DuckDB version under a fixed `extension_directory`, with
`autoinstall_known_extensions=false` (as the PoC runs) [P]. A DuckDB upgrade is an upgrade of crate,
extensions and catalog format together, gated by this PoC's suite.

---

## 15. PoC evidence summary

| ID | Result | Key numbers |
| :--- | :--- | :--- |
| E1 | PASS | stale ETag → 412; 1 of 8 racing lease writers wins |
| E2 | PASS | 44 files + 11 envelopes in 783 ms, no DuckDB; crash lost 21,133 spans / 12,063 samples |
| E3 | PASS | 2 buckets, 1 catalog; commit p50 41.9 ms; replay skipped 1; bad envelope fully rolled back |
| E4 | PASS | 5/5 spans, 2 regions, 2 flush windows; bloom halves bytes (294.7 vs 623.8 KiB); v5 layout worst (38 GETs) |
| E5 | PASS | 0 series off on read-time and compactor paths; v5 rule −9.33 %; L0 6.18 / L1 4.60 bytes/sample |
| E6 | PASS | atomic swap 11 → 2 files; change feed 178,867 ins + 178,867 del; DuckLake deleted 11/11 external L0 |
| E7 | PASS | watermark correct in 3 states; naive union +60,000; Arrow vtab 115.9 ms / 60k rows |
| E8 | PASS | p50/p95/p99 worst error 1.01 %; 1,339 inlined partial rows; panel read p50 12.5 ms |
| E9 | PASS | snapshot attach 44 ms, lookup 28 ms, write refused; needs flush of inlined rows first |

---

## 16. Risks and what the PoC did not prove

| Risk | Why it matters | First check |
| :--- | :--- | :--- |
| Scale | PoC ≈ 200k spans on local MinIO; real S3 adds 20–100 ms first-byte per GET | Sprint 1: 1 day × 1 region on real S3/R2, measure lookup and range queries |
| Bytes/sample | L1 is 4.60 B/sample on synthetic random-walk counters, far from v5's "< 1.1" | Real exporter data; try dropping `region`/`metric_name` from points (they live in `metrics_series`) |
| Catalog growth | 1 snapshot per tick; `applied_envelopes` grows forever | Batched apply + hourly expiry + trimming `applied_envelopes` to max seq per epoch; soak test for 7 days |
| Swap vs concurrent add | not tested | Single coordinator writes the catalog; add a test that runs both in parallel |
| Hot-tail cost | Arrow vtab 115.9 ms for 60k rows | Compare per-node DuckDB in-memory table |
| Laptop export | depends on DuckLake internals | E9 in CI on every DuckLake upgrade |
| Extension coupling | extensions pinned to DuckDB version; public host may be blocked | Ship extensions in the image; one upgrade path |
| PromQL | M1 relies on an external Prometheus | Compliance tester on the M1 front; plan native subset |
| Durability modes | only `memory` was exercised | `wal` implementation + kill -9 test in M1 |
| Nested types | `events`/`links` lists of structs untested through `add_data_files` | Sprint 1 |

---

## 17. Implementation plan

Estimates assume 2–3 engineers.

| Milestone | Scope | Exit test |
| :--- | :--- | :--- |
| **M1 (6–8 wk)** single-node metrics | remote_write + OTLP metrics receivers; `wal` durability; writer/flusher/envelopes; checkpointer (batched); metrics compactor + retention; remote_read + metadata API; Prometheus-front reference deployment; Grafana dashboards work | kill -9 loses no acked sample; remote_read results equal a reference Prometheus on the same input |
| **M2 (6–8 wk)** traces + logs + agents | OTLP traces/logs, Loki push; span compactor (Rust writer); `traces_index`; Tempo + Loki read subsets; TQS v2.1; MCP read tools; laptop export; auth tokens | E4/E9-style checks on real S3; MCP incident scenarios |
| **M3 (4–6 wk)** cluster | roles; lease + fencing; hot-tail RPC; `replicated` mode; threat model | partition/kill tests; no double count (E7 across processes) |
| **M4 (4–6 wk)** panels + native PromQL subset | materializer from envelopes; view_partials; native PromQL subset with pushdown | 1,000 panels; compliance tester score published |
| **M5 (~4 wk)** multi-region | per-region buckets + secrets; catalog replicas; residency | cross-region trace + counter totals exact |

### 17.1 First sprint backlog (turns the PoC into M1's skeleton)

1. Workspace layout (§14); move `write.rs`/`lake.rs` logic into `core`/`write`/`catalog`, with the
   schema generated once.
2. Prometheus remote_write receiver feeding the flusher; metrics_series per-day rows.
3. Batched checkpointer (all pending envelopes in one transaction) + lease/fence.
4. `wal` durability mode + a kill -9 test.
5. Metrics compactor with the cross-hour seed; retention loop with expire + cleanup.
6. `remote_read` endpoint; docker-compose with MinIO, PostgreSQL and a Prometheus front.
7. Port `scripts/run-poc.sh` experiments into an integration suite that runs on every PR.
8. Run E2–E6 at 100× scale on real S3 and record the numbers in this document.

---

## 18. Decisions the team still owns

1. Default durability mode, which sets the RPO users get out of the box.
2. Whether v1 relies on an external Prometheus for PromQL (D11), or a native subset is a v1 gate.
3. Product and binary name (the PoC uses `blobobs`).
4. Primary target store: S3 or R2. This decides which benchmark comes first.
5. Tenancy in v1: single tenant with the `tenant` column in place, or real multi-tenant from M3.
