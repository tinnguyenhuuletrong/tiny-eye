Observability Engine SPEC 1.0 · TQS 3

[Overview](#overview) [Specification](#spec) [Research & notes](#research) [Questions (20)](#questions) [Download](#package)

# Blob-first observability engine

Product specification v1.0 · Rust · DuckDB 2.0 + DuckLake + S3 · research run 2026-11-10

One Rust binary stores metrics, logs and traces as Parquet in object storage and answers dashboards, alerts and AI agents with TQS 3, a declarative JSON query schema. Every claim in the specification is tied to a test that ran: storage experiments on MinIO and DuckLake, and query cases checked point by point against Prometheus on the same data.

9/9

storage experiments on MinIO + DuckLake

33/33

metric functions equal to Prometheus 3.16

17/17

real Margince panel runs equal to Prometheus

30,758

points compared with Prometheus, all equal

14/14

spans, logs and trace cases equal to an independent computation

38/38

validation and injection cases

15/15

deliberate compiler breaks caught by the suite

98/99

real Margince dashboard panels expressible in TQS 3

## Read in this order

- [Specification](#spec): scope, architecture, storage, ingest, and the full TQS 3 reference (§8).
- [Questions](#questions): 20 decisions with a proposed default each. Only Q7 and Q11 block milestone 1.
- [Research & notes](#research): method, every result, and what the research does not prove.

## Evidence tags

- E1–E9 storage experiment on MinIO + DuckLake
- M01 R05 query case equal to Prometheus 3.16
- S3 L4 T1 equal to an independent Rust computation
- V12 refused with a named problem code
- P one-off probe · U not yet verified

## Not proven yet

- Performance on real S3 and at production scale
- Durability modes other than in-memory
- Hot-tail reads across processes
- All 99 real panels numerically (11 shapes proven; 98 expressible)

Contents

1. [1. Product](#)
2. [2. Glossary](#)
3. [3. Architecture](#)
4. [4. Storage](#)
5. [5. Ingest](#)
6. [6. Catalog, compaction, retention (coordinator)](#)
7. [7. Query execution](#)
8. [8. TQS 3 — the query schema (reference)](#)
9. [9. Dashboards and alerts](#)
10. [10. AI agents (MCP)](#)
11. [11. Security and privacy](#)
12. [12. Multi-region](#)
13. [13. Operability](#)
14. [14. Edge cases](#)
15. [15. Implementation plan](#)
16. [16. Questions for the product owner](#)

# Blob-First Observability Engine — Product Specification

**Version:** 1.0 (final for implementation start) · **Date:** 2026-11-10 · **Language:** Rust **Supersedes:** the v5 "Master Architecture & Product Specification", its review, and Concept v1 (kept under `history/`).

Every claim in this document carries an evidence tag. The evidence is in [RESEARCH.md](RESEARCH.md) and is reproduced by `research/scripts/run-all.sh`.

| Tag | Meaning |
| --- | --- |
| **E1…E9** | Storage experiment on MinIO + DuckLake (`research/results/poc-results.md`) |
| **M01…M33, D1–D2, R01…R11** | Query case equal to Prometheus 3.16 on the same data (`research/results/tqs-coverage.md`) |
| **S1…S6, L1…L5, T1…T3** | Spans/logs/trace case equal to an independent Rust computation |
| **V01…V36, A1–A2** | Validation case: the query is refused with the stated problem code, or is harmless |
| **MUT** | Mutation check: breaking that rule in the compiler turns at least one case red |
| **P** | One-off probe during the research, not a scripted test |
| **U** | Unverified: a design decision the research did not exercise |

Open decisions are marked **→ Q*n*** and collected in §16 for the product owner to answer.

---

## 1. Product

### 1.1 Problem

Running Prometheus + Loki + Tempo (+ Mimir/Thanos) means many services, three query languages and local disks to babysit. Single-binary tools stay on one disk. Teams want cheap, long retention in object storage, one query model for metrics, logs and traces, and safe access for AI agents.

### 1.2 What we build

One Rust binary that: - ingests OTLP (metrics, logs, traces), Prometheus remote-write and Loki push; - stores everything as sorted ZSTD Parquet in S3-compatible object storage, tracked by a DuckLake catalog; - answers dashboards, alerts and agents with **TQS 3**, our declarative JSON query schema, compiled to DuckDB SQL.

**No PromQL endpoint.** TQS provides the dashboard functions PromQL users rely on, with the same numbers M01–M33, R01–R11. Users write TQS panels, not PromQL.

### 1.3 Users

| User | Needs |
| --- | --- |
| Operator / on-call engineer | Dashboards and alerts that show the same numbers they trust today |
| Developer | Trace waterfall, log search, "what changed" in one place |
| AI agent (MCP) | A closed schema it cannot get wrong silently; bounded cost; no write access by default |
| Platform owner | Cheap retention, few moving parts, data residency per region |

### 1.4 v1.0 scope

**In:** metrics, logs, traces; OTLP gRPC/HTTP, Prometheus remote-write, Loki push; TQS 3 with all constructs in §8; dashboards and alert rules expressed in TQS; MCP tools (read + one admin tool); single-node and cluster modes; one bucket per region.

**Out of v1.0:** PromQL/LogQL/TraceQL endpoints; Prometheus scrape mode (users keep a Prometheus agent, the OTel Collector or Alloy as the scraper); historical importers; RUM; the AI dashboard migrator; multi-tenant isolation beyond a `tenant` column.

### 1.5 Success criteria for v1.0

1. Every panel of the Margince Grafana board is expressible in TQS. Today: 98 of 99 at construct level, with the numbers proven for 11 representative panel shapes R01–R11.
2. A wired test against a running Margince stack shows TQS panels equal to Prometheus on the same scrape (§15.3).
3. A `kill -9` of any ingest node loses no acknowledged data in the default durability mode (§5.2).
4. Trace lookup by id over 1 h of data, on real S3, under a target set after the first benchmark (§15.2).

---

## 2. Glossary

| Term | Meaning |
| --- | --- |
| L0 / L1 file | Parquet file from one ingest flush / from hourly compaction |
| Envelope | 1 KB JSON object that makes a flush's files visible once the catalog applies it |
| Checkpoint | One catalog transaction that applies pending envelopes |
| Coordinator | The single leased role per region that writes the catalog |
| Hot tail | Rows an ingest node holds in RAM that the catalog does not yet include |
| Series | One metric name + one label set; `series_id` is its hash |
| Step | Spacing of the points of a time series result |
| Window | Look-back span of a range function: the point at `t` reads `(t − window, t]` |
| Key | Canonical label string of a result series: `k=v` pairs, sorted, comma-joined, empty values omitted |

---

## 3. Architecture

```text
 OTLP / remote_write / Loki push                       Dashboards (TQS) · Alert rules · MCP · laptop
              │                                                            │
              ▼                                                            ▼
 ┌──────────────────────────────┐   hot-tail RPC (watermarked)   ┌──────────────────────────────┐
 │ INGEST role (N, stateless)   │◄───────────────────────────────│ QUERY role (N, stateless)    │
 │ decode → arena → durability  │                                │ TQS validate → compile →     │
 │ → sort → Parquet → PUT       │                                │ DuckDB + hot tails + caches  │
 │ → envelope (If-None-Match)   │                                └──────────────┬───────────────┘
 └──────────────┬───────────────┘                                               │ read
                ▼                                                               ▼
 ┌─────────────────────────────────────────────────────────────────────────────────────────────┐
 │ OBJECT STORAGE, one bucket per region: telemetry-<region>/{spans,logs,metrics_*,commits}/…  │
 └──────────────▲──────────────────────────────────────────────────────────────▲───────────────┘
 ┌──────────────┴───────────────────────────────────────────────┐              │ catalog snapshot
 │ COORDINATOR role (1 per region, leased + fenced)             │──────────────┘ (laptop mode)
 │ checkpointer · compactor · retention · materializer ·        │
 │ alert evaluator · snapshot exporter                          │
 └──────────────┬───────────────────────────────────────────────┘
                ▼
 ┌──────────────────────────────────────────────┐
 │ DuckLake catalog (PostgreSQL in cluster mode;│
 │ DuckDB file synced to S3 in single-node mode)│
 └──────────────────────────────────────────────┘
```

One binary, roles by flag: `--role=all` (single node), `ingest`, `query`, `coordinator`, `mcp --read-only`.

### 3.1 Decisions

| # | Decision | Evidence |
| --- | --- | --- |
| D1 | Rust. The ingest path uses `arrow`/`parquet`/`object_store` only; DuckDB (bundled `duckdb` crate) serves query, coordinator and laptop roles. | E2 |
| D2 | No ingest-side state for counters and no routing. Increase is computed at read time over L0 and stored as `positive_delta` by the compactor in L1. | E5: exact on both paths; the v5 ingest-state rule lost 9.33 % under a crash |
| D3 | DuckLake is the catalog. We write files and register them; DuckLake then owns their lifecycle. | E3, E6 |
| D4 | Our own compactor (sort + bloom), swapping files in one catalog transaction. | E6, P DuckDB's writer emits no `trace_id` bloom |
| D5 | Materialized panels are fed from our envelopes, never the DuckLake change feed. | E6 |
| D6 | Percentiles in stored partials are log-bucket histograms merged with `SUM`. | E8 ≤ 1.01 % error; P t-digest state is not storable |
| D7 | Conditional writes for envelopes and the lease; every catalog write is fenced. | E1 |
| D8 | Per-node watermark for hot tails, read in the scan's catalog transaction. | E7 |
| D9 | One catalog, one bucket per region, absolute file paths. | E3, E4 |
| D10 | Laptop mode reads a catalog snapshot file from S3. | E9 |
| D11 | **TQS 3 is the only query language.** Metric functions reproduce Prometheus semantics. | M01–M33, R01–R11, MUT |
| D12 | Ship DuckDB extensions with the binary, pinned to the bundled DuckDB version. | P the extension host was unreachable |
| D13 | Apache-2.0; no AGPL code. | U |

---

## 4. Storage

### 4.1 Object keys

```
s3://telemetry-<region>/
  commits/<region>/<ulid>-<node_id>.json                 envelopes; deleted after their checkpoint
  catalog/<tenant>/coordinator.lease                     lease (conditional PUT)
  catalog/<tenant>/snapshot-latest.ducklake              laptop/agent catalog snapshot
  spans/tenant=<t>/date=<d>/hour=<h>/l0-<ulid>-<node>.parquet
  spans/tenant=<t>/date=<d>/hour=<h>/l1-<ulid>-compacted.parquet
  logs/…  metrics_points/…  metrics_series/…  metrics_hist/…            (same pattern)
```

DuckLake stores paths and column statistics; it does not parse the hierarchy E3. The hierarchy serves humans, lifecycle backstops and residency audits.

### 4.2 Parquet writer settings

| Setting | Value | Evidence |
| --- | --- | --- |
| Compression | ZSTD level 3 | E2 |
| Statistics | page level, page index written | E2 |
| Row group | 122,880 rows | U — the research used 4,096–8,192 on small files |
| `trace_id` | dictionary off; split-block bloom filter, FPP 0.01 | E4; P present on all L1 row groups |
| `timestamp_ms` | `DELTA_BINARY_PACKED` | E2 |
| metric `value` | `BYTE_STREAM_SPLIT` | E2 |

### 4.3 Tables

The Arrow schema the writer emits and the DuckLake DDL are generated from one definition: `add_data_files` refuses a mismatch only at commit time E3. Proven type mapping: Arrow `FixedSizeBinary(16)` → `BLOB`; `UInt64` → `UBIGINT`; `UInt8` → `UTINYINT`; `Timestamp(ns)` → `TIMESTAMP_NS`; `Map<Utf8,Utf8>` → `MAP(VARCHAR, VARCHAR)` E3.

```sql
CREATE TABLE spans (tenant VARCHAR, region VARCHAR, trace_id BLOB, span_id UBIGINT, parent_span_id UBIGINT,
  start_time_ns TIMESTAMP_NS, duration_ns UBIGINT, service_name VARCHAR, span_name VARCHAR,
  span_kind UTINYINT, status_code UTINYINT, status_message VARCHAR,
  resource_attrs MAP(VARCHAR, VARCHAR), span_attrs MAP(VARCHAR, VARCHAR),
  events STRUCT(ts TIMESTAMP_NS, name VARCHAR, attrs MAP(VARCHAR, VARCHAR))[],
  links STRUCT(trace_id BLOB, span_id UBIGINT, attrs MAP(VARCHAR, VARCHAR))[]);      -- sort (trace_id, start_time_ns)
CREATE TABLE traces_index (tenant VARCHAR, region VARCHAR, trace_id BLOB, has_root BOOLEAN,
  root_service_name VARCHAR, root_span_name VARCHAR, root_status_code UTINYINT,
  min_start_ns TIMESTAMP_NS, max_end_ns TIMESTAMP_NS, span_count UINTEGER, error_count UINTEGER,
  services VARCHAR[]);                                                                 -- one row per (trace, file)
CREATE TABLE metrics_series (tenant VARCHAR, region VARCHAR, day DATE, series_id UBIGINT,
  metric_name VARCHAR, metric_type UTINYINT, unit VARCHAR, labels MAP(VARCHAR, VARCHAR)); -- one row per (series, day)
CREATE TABLE metrics_points (tenant VARCHAR, region VARCHAR, series_id UBIGINT, timestamp_ms BIGINT,
  value DOUBLE, is_stale BOOLEAN, positive_delta DOUBLE);                              -- sort (series_id, timestamp_ms)
CREATE TABLE logs (tenant VARCHAR, region VARCHAR, timestamp_ns TIMESTAMP_NS, service_name VARCHAR,
  severity_number UTINYINT, severity_text VARCHAR, pattern_hash UBIGINT, pattern_version UTINYINT,
  body VARCHAR, trace_id BLOB, span_id UBIGINT, resource_attrs MAP(VARCHAR, VARCHAR),
  log_attrs MAP(VARCHAR, VARCHAR));                                                    -- sort (service_name, severity_number, timestamp_ns)
CREATE TABLE applied_envelopes (envelope_id VARCHAR, region VARCHAR, node_id VARCHAR,
  node_epoch BIGINT, seq_to BIGINT, files INTEGER);
CREATE TABLE view_partials (tenant VARCHAR, panel_id VARCHAR, panel_version INTEGER, bucket_ts BIGINT,
  region VARCHAR, group_key VARCHAR, agg_name VARCHAR, n UBIGINT, s DOUBLE, mn DOUBLE, mx DOUBLE,
  last_ts BIGINT, last_val DOUBLE, hist_bucket SMALLINT);                              -- inlined, scalar columns only
```

- `events`/`links` (lists of structs) through `add_data_files` are U; test them in the first sprint.
- Classic Prometheus histograms are stored as ordinary series (`_bucket{le}`, `_count`, `_sum`); this is the shape `histogram_quantile` was proven on M06–M08, R03, R07. Native/exponential histograms go to a `metrics_hist` table and are U for querying in v1.
- A label with an empty value is the same as an absent label (Prometheus semantics) \[R10, `container!=""`\].

### 4.4 Series identity

```
series_id = xxh64( for part in [tenant, region, metric_name, canonical_labels]:
                     u32_le(len(part)) ‖ part )
canonical_labels = labels sorted by key, each u32_le(len(k)) ‖ k ‖ u32_le(len(v)) ‖ v
```

Length-prefixing makes the encoding injective; tenant and region come from the receiving node's config, never the payload E2. On a known id with different labels, the engine increments `series_id_collisions_total` and logs both label sets U.

---

## 5. Ingest

### 5.1 Path

1. Decode: OTLP (`opentelemetry-proto` + `tonic`), remote-write (`prost` + `snap`), Loki push. Reject samples older than 1 h or more than 10 min ahead (configurable) U.
2. Admit into a byte-accounted arena. At 75 %, spill frozen batches to ephemeral disk; at 95 %, return `429` / `RESOURCE_EXHAUSTED` U.
3. Persist according to the durability mode (§5.2), then acknowledge.
4. Flush at 64 MB or 60 s: sort, write Parquet in memory, `PUT` E2.
5. Commit one envelope per flush with `If-None-Match: *` E1, E2.
6. Keep the batch in RAM until the catalog shows it committed plus a grace period (hot tail, §7.3).

No routing: a load balancer may spray requests E2, E5.

### 5.2 Durability modes

| Mode | Ack after | Lost on one node crash | Evidence |
| --- | --- | --- | --- |
| `memory` | arena admission | that node's unflushed batch; cumulative counter totals survive | E2, E5 |
| `wal` | group-committed fsync to local disk, replayed on restart | nothing while the disk survives | U |
| `replicated` | batch held by 2 in-region peers | needs 2 failures in one flush interval | U |
| `s3-wal` | ≤ 1 s segment PUT to object storage | nothing | U |

Proposed default: `wal` for single node, `replicated` for cluster [→ Q7](#questions).

### 5.3 Envelope

```json
{ "format": 1, "envelope_id": "<ulid>", "kind": "ingest", "tenant": "default", "region": "us-east",
  "node_id": "us-east-n0", "node_epoch": 1, "seq_to": 4,
  "files": [ { "table": "spans", "path": "s3://telemetry-us-east/spans/…/l0-…parquet", "bytes": 694512, "rows": 16743 } ] }
```

`node_epoch` changes on every process start; `seq_to` restarts per epoch E2, E7.

### 5.4 Log patterns

`pattern_hash` = xxh64 of the body after a fixed, versioned masking: UUIDs, IPv4 (with port), hex runs ≥ 8, double-quoted strings, then numbers become `<*>`. It is deterministic on every node and region. On the fixtures it reduced 4,288 lines to exactly the 5 planted templates L4. Learned (Drain-style) clustering is not used for the hash, because it differs per node.

---

## 6. Catalog, compaction, retention (coordinator)

### 6.1 Checkpointer

```text
every 5 s, holding the lease:
  list commits/<region>/ for every region; parse; sort by ulid
  BEGIN
    for env in pending:
      if applied_envelopes has env.id: mark done; continue           -- crash replay
      for f in env.files: CALL ducklake_add_data_files('lake', f.table, f.path)
      INSERT INTO applied_envelopes VALUES (…)
  COMMIT                                                             -- one snapshot per tick
  delete applied envelope objects
```

Proven per envelope: - a missing file aborts the whole envelope E3; - a crash between `COMMIT` and the delete creates no duplicates E3; - commit takes p50 ≈ 42 ms with 4 files on PostgreSQL E3.

Batching all pending envelopes into one transaction per tick is U.

### 6.2 Lease and fencing

The lease object is created with `If-None-Match: *` and renewed with `If-Match: <etag>` (10 s renew, 30 s TTL). In E1, a stale ETag was refused and 1 of 8 racing writers won.

Every catalog transaction also updates `coordinator_fence(epoch)` with `WHERE epoch <= :mine`. Zero rows updated means another coordinator holds the epoch, so the transaction aborts and this coordinator steps down U.

Only the coordinator writes the catalog. Concurrent writers work: 4 × 10 inserts gave 0 errors, with a worst case of 972 ms P. A compaction swap racing an ingest add on the same table was not tested U.

### 6.3 Compaction

This runs per `(tenant, region, table, hour)`, after the hour closes plus 10 min.

- **Spans and logs:** read L0 sorted, write L1 with the Rust writer (bloom), then in one transaction `DELETE … WHERE filename IN (L0…)` + `add_data_files(L1)` E6.
- **Metrics:** DuckDB `COPY` computes `positive_delta` with a window over `value`, then the same swap E5. The previous hour's last point seeds the window U.
- **Effect:** 11 L0 → 2 L1 took a trace lookup from 26 GETs to 6 E6.

### 6.4 Retention

`DELETE … WHERE filename IN (expired)`, then `ducklake_expire_snapshots` and `ducklake_cleanup_old_files`. DuckLake deletes the objects, including ones we wrote outside its data path and in other buckets E6.

Rules that follow: - the coordinator needs delete rights in every regional bucket; - an object is never registered twice; - S3 lifecycle rules are only a backstop, set to retention + 7 days.

Default retention is [→ Q14](#questions).

---

## 7. Query execution

### 7.1 Engine

- TQS JSON is validated (§8.12), then compiled to one DuckDB SQL statement. Every string from the query is a bound parameter. Only validated numbers and identifiers are formatted in A1, A2, V16, V22, V36.
- One DuckDB connection per worker thread; queries run on blocking threads.
- Every query runs in one read transaction, so the catalog snapshot it pins is consistent E7.
- Per query: `memory_limit`, `threads`, timeout, max bytes scanned U.
- Production enables `enable_external_file_cache` and `parquet_metadata_cache` \[P: the settings exist in DuckDB 2.0\].
- The same compiled SQL gives identical results on in-memory tables and on DuckLake tables in MinIO \[tqs-lake: 6/6\].

### 7.2 Trace lookup cost

DuckLake prunes files by min/max, and random trace ids defeat that. Each file therefore costs a footer GET plus a bloom GET E4: 22 GETs for 11 files with an absent id.

| Layout, 11 L0 files | GETs | KiB | ms |
| --- | --- | --- | --- |
| sorted by `trace_id` + bloom (chosen) | 26 | 294.7 | ≈ 22 |
| sorted, no bloom | 22 | 623.8 | ≈ 37 |
| v5: service first + bloom | 38 | 509.6 | ≈ 40 |
| chosen, after compaction (2 L1 files) | 6 | 155.6 | ≈ 15 |

Rules: - a trace lookup always carries a time range (default ±1 h); - compaction is hourly; - a trace-id → file index is a later option U.

### 7.3 Hot tail

1. The query pins snapshot S.
2. It reads `W[node, epoch] = max(seq_to)` at S.
3. Each node returns batches with `seq > W`.
4. If a node has already dropped rows that S lacks, the query re-pins (≤ 2 tries), then warns U for the re-pin.

Proven in-process: correct before flush, after commit and after drop. A naive union double-counts 60,000 rows E7.

Scanning a 60k-row Arrow batch through DuckDB's `arrow()` table function took ≈ 116 ms, because it has no projection pushdown. The alternative to measure in M1 is a per-node in-memory DuckDB table U.

### 7.4 Laptop / agent mode

`blobobs mcp --read-only --catalog s3://…/catalog/<tenant>/snapshot-latest.ducklake`.

The coordinator exports the snapshot every 5 min (default) E9: 1. Flush inlined rows (`ducklake_flush_inlined_data`). This is required: PostgreSQL stores inlined rows in backend-specific types, so copying them makes an unreadable DuckDB catalog. 2. Copy the `ducklake_*` metadata tables except `ducklake_inlined_data_*`. 3. Create an empty `ducklake_inlined_data_tables`. 4. PUT with `If-Match`.

Measured: attach 44 ms, trace lookup 28 ms, writes refused E9. This depends on DuckLake internals, so it is gated by E9 in CI on every DuckLake upgrade. Freshness is export interval + flush interval, with no hot tail [→ Q19](#questions).

---

## 8. TQS 3 — the query schema (reference)

TQS 3 is the one query interface: dashboards, alert rules, the MCP tool and the HTTP API all accept it. It is JSON. Unknown fields are refused V01.

### 8.1 Document

| Field | Type | Required | Notes |
| --- | --- | --- | --- |
| `tqs` | `"3"` | yes | Other versions → `version` V02 |
| `id` | string `^[a-z][a-z0-9_-]{0,31}$` | yes |  |
| `title` | string | no | Display only |
| `kind` | `timeseries` \| `table` \| `trace_search` \| `trace` \| `log_search` \| `log_patterns` | yes |  |
| `time` | object (§8.2) | yes, except `trace` |  |
| `vars` | object `name → string` | no | §8.3 |
| `queries` | array of named queries, 1–10 | for `timeseries`/`table` | §8.4–8.7 |
| `formulas` | array | no | §8.8 |
| `top` | object | no | §8.9 |
| `search`, `trace_id`, `limit` |  | for list kinds | §8.10 |

### 8.2 Time

- `from`, `to`: epoch ms, `"now"`, or `"-<duration>"` (e.g. `"-2h"`, relative to now). `to` defaults to `"now"`. `from` must be before `to` V04.
- `step`: duration (`ms`, `s`, `m`, `h`, `d`, `w`). Default: range / 300, rounded up to a whole second, at least 1 s.
- At most **11,000 points** per series V05.
- `timeseries`: points at `from, from + step, …, ≤ to`.
- `table`: metrics are evaluated at `to`; spans/logs aggregate over `(from, to]` M30, S5, L2.

### 8.3 Variables

`vars: {"namespace": "ns-a"}`. Any string value written as `"$name"` (matcher value, condition value, window) is replaced by the variable and **bound as a parameter**. Unknown variables are refused (`var`).

Built-ins: - `$__range` = `to − from`, as a window; - `$__interval` = step.

Proven with `$namespace` and `$__range` on real panels R01–R11. `$__rate_interval` is not provided [→ Q15](#questions).

### 8.4 Metrics query

```json
{ "name": "err", "metric": "margince_ai_calls_total",
  "match": [ { "label": "outcome", "op": "=", "value": "error" },
             { "label": "kubernetes_namespace", "op": "=", "value": "$namespace" } ],
  "fn": { "op": "rate", "window": "5m" },
  "agg": { "op": "sum", "by": ["provider"] },
  "offset": "30m", "having": { "op": ">", "value": 0 }, "fill": "zero", "hidden": false }
```

**Selection (`match`).** Ops `=`, `!=`, `=~`, `!~`: - regexes are RE2 and fully anchored (PromQL semantics), ≤ 256 characters M21, V18, V19; - an absent label equals `""` R10; - label values may contain any character, quotes included M28.

**Range functions (`fn`).** The point at `t` reads samples in `(t − offset − window, t − offset]` MUT closed_window.

| `fn.op` | Meaning (Prometheus-identical) | Evidence |
| --- | --- | --- |
| `rate` | Per-second increase with counter-reset correction and Prometheus boundary extrapolation | M01, M02, MUT no_reset_correction, no_extrapolation |
| `increase` | `rate × window` | M03, R01 |
| `irate` | From the last two samples | M04 |
| `delta` | Gauge difference, extrapolated, no reset correction | M18 |
| `resets`, `changes` | Counts in the window | M24, M25 |
| `last` | Latest sample within 5 min lookback (no window) | M10, R05, MUT lookback_4m |
| `avg/min/max/sum/count/last_over_time` | Over the window | M12–M17 |
| `histogram_quantile` | `q` in \[0, 1\]; reads `<metric>_bucket`; rate per bucket, sum per group and `le`, monotonic fix, linear interpolation, +Inf bucket → the highest finite bound | M06–M08, R03, R07, MUT hist_upper_bound |

A function needing ≥ 2 samples gives no point with fewer. A window shorter than the scrape interval therefore gives an empty result, exactly as in Prometheus M31.

**Aggregation (`agg`).** - `op`: `sum`, `avg`, `min`, `max`, `count` (series with a value), `absent`; - `by: [labels]` or `without: [labels]` (exclusive V25); - `op` is required V26; - with neither `by` nor `without`, everything collapses into one series; - without `agg`, each series is kept M02, M22, M23, M29.

`absent` gives `1` at steps with no series, labelled with the query's `=` matchers (PromQL `absent`) R04z, MUT absent_unlabelled.

**Second aggregation (`then`).** It aggregates the groups of `agg`, after `having`. For example, PromQL `sum by (queue, kind) (max by (queue, kind, workspace_id) (…))` is `agg: {op: max, by: [queue, kind, workspace_id]}, then: {op: sum, by: [queue, kind]}`. `then.by` must be a subset of `agg.by` R05, R09, MUT then_ignored.

**Derived labels (`derive`).** PromQL `label_replace`, applied in order before aggregation: `{label, from, regex, replacement}`. When `regex` fully matches the value of `from`, `label` is set to `replacement` with `$1`–`$9` expanded; otherwise the labels are unchanged R10, MUT derive_ignored.

**`offset`** shifts the evaluation into the past M20. **`having`** keeps points satisfying `op` (`=`, `!=`, `>`, `>=`, `<`, `<=`) against a number M27. **`hidden: true`** computes but does not return the query R07.

**`fill: "zero"`** has two forms D1, R01, R02, R09, S4, MUT fill_ignored: - for a query that aggregates to one series, it emits `0` at every step without a value, even when nothing matched (PromQL `… or vector(0)`); - for a grouped query, it fills the keys it has seen, and as a formula operand a missing key counts as `0`.

### 8.5 Spans query

```json
{ "name": "e", "spans": [ { "field": "status_code", "op": "=", "value": 2 } ],
  "measure": { "op": "count" }, "agg": { "by": ["service_name"] }, "fill": "zero" }
```

- Fields: `service_name`, `span_name`, `span_kind`, `status_code`, `duration_ms`, `region`, `trace_id`, `attr.<key>`, `resource.<key>`.
- Ops: `=`, `!=`, `>`, `>=`, `<`, `<=` (numeric fields), `=~`, `!~`, `contains`, `in` (≤ 100 values), `exists` (`attr.*` / `resource.*`).
- Numeric comparisons on text fields are refused V28; unknown fields too V27.
- `measure.op`: `count`, `rate` (count / step seconds), `quantile` (exact, `q` required V29), `avg`, `min`, `max`, `sum`. All but count/rate need `field: "duration_ms"`.
- Bucketing: the point at `t` covers spans whose **start** is in `(t − step, t]` S1–S6, MUT bucket_closed_left [→ Q3, Q4](#questions).

### 8.6 Logs query

Same shape with `logs`. Fields: `service_name`, `severity_number`, `severity_text`, `body`, `pattern_hash`, `trace_id`, `region`, `attr.<key>`, `resource.<key>`. `measure.op` is `count` or `rate` L1, L2, V30.

### 8.7 Grouping keys

A result series is identified by its key: the grouped labels as `k=v`, sorted, comma-joined, empty values omitted. It is `""` for a single series. The key is part of the API response; a structured label map is returned next to it U for the map.

### 8.8 Formulas

```json
"formulas": [ { "name": "pct", "expr": "100 * least(e / greatest(a, 1), 1)" },
              { "name": "depth", "expr": "coalesce(a, b * 0, c * 0)", "join": "outer" } ]
```

Grammar (closed; SQL is generated from the AST) V21–V23:

```ebnf
expr  = term { ("+" | "-") term } ;      term  = unary { ("*" | "/") unary } ;
unary = "-" unary | atom ;               atom  = number | name | "(" expr ")" | func "(" expr { "," expr } ")" ;
func  = abs | round | least | greatest | clamp | coalesce ;
```

- **Names:** queries, earlier formulas, and `time`, which is the evaluation time in seconds (PromQL `time()`) R06, MUT time_in_ms. At most 512 characters and 128 nodes.
- **Division by zero yields no value** (a gap), not ±Inf/NaN D2, MUT division_ieee [→ Q2](#questions).
- **Join `inner`** (default): a point exists where every vector operand has one, matched on step and key M05, M33.
- **Join `outer`**: the union of points, and a missing operand is NULL. So `coalesce(a, b * 0, c * 0)` is PromQL `a or b * 0 or c * 0` R05, MUT outer_as_inner.
- A query that aggregates to one series broadcasts to every key M26.
- Vector operands must be grouped the same way; otherwise the formula is refused (`formula_join`) V24.
- An aggregation after a formula is not supported. Rewrite it before the formula, as in `max by (app) (time() − x)` = `time − min by (app)(x)` R06 [→ Q6](#questions).

### 8.9 Top

- `top: {n: 1..1000, rank: "per_step" | "max" | "avg" | "last", order: "desc" | "asc"}` applies to every returned output. `per_step` is PromQL `topk` M19; other ranks pick n series for the whole range U.
- `top.query: "<name>"` ranks by another (usually hidden) query at each step and keeps only those keys. That is PromQL `x and on(k) topk(n, y)` R07, MUT top_query_ignored.

### 8.10 List kinds

| Kind | Input | Output columns | Evidence |
| --- | --- | --- | --- |
| `trace_search` | `search.spans` (one span matches all); `search.trace`: `duration_ms`, `has_error`, `span_count`, `root.service_name`, `root.span_name`, `service`; `order`: `duration_desc` \| `start_desc`; `limit` | `trace_id`, `start_ms`, `duration_ms`, `span_count`, `has_error`, `root_service`, `root_span`, `services` | T1 |
| `trace` | `trace_id` (32 lowercase hex V31) | `depth`, `region`, `service_name`, `span_name`, `offset_ms`, `duration_ms`, `self_ms`, `status_code`, `is_root`, `orphan` | T2, T3 |
| `log_search` | `search.logs`; `order`: `desc` \| `asc`; `limit` | `ts_ms`, `service_name`, `severity_text`, `body`, `trace_id` | L3, L5 |
| `log_patterns` | `search.logs`; `limit` | `pattern_hash`, `n`, `example`, `services` | L4 |

- `limit` is 1–5000 (default 100).
- Trace search sees spans whose start is in the range. A trace that crosses the range edge shows only its in-range spans U.
- A span whose parent is missing is drawn at depth 0 and flagged `orphan` T3.

### 8.11 Response

```json
{ "tqs": "3", "id": "…", "snapshot_id": 1234, "freshness_s": 4.2, "partial_regions": [],
  "series": [ { "output": "pct", "key": "provider=openai", "labels": {"provider": "openai"},
                "points": [[1762743600000, 4.25], …] } ],
  "rows_total": 172, "truncated": false, "bytes_scanned": 1048576 }
```

The engine's internal result is `(output, t, key, value)` rows, which is what the research proves. The envelope fields above are U.

### 8.12 Problem codes

`validate` returns **every** problem in one response, each with `code`, JSON `path` and `message`, so an agent fixes all of them in one round trip.

| Code | When | Case |
| --- | --- | --- |
| `schema` | JSON shape, unknown field, unknown enum value | V01, V35 |
| `version` | `tqs` ≠ `"3"` | V02 |
| `id`, `name` | bad or duplicate names; `time` is reserved | V09 |
| `time`, `time_order`, `step`, `too_many_points` | time problems | V03–V06 |
| `queries`, `source` | 0 or > 10 queries; not exactly one of metric/spans/logs | V07, V08, V10 |
| `metric`, `label`, `op`, `regex`, `var` | selection problems | V16–V19, V36 |
| `fn`, `window`, `q` | function problems | V11–V15 |
| `agg`, `then`, `derive`, `having`, `offset` | shaping problems | V25, V26, V32, V33 |
| `measure`, `field`, `cond` | spans/logs problems | V27–V30 |
| `formula`, `formula_ref`, `formula_join` | formula problems | V20–V24 |
| `top`, `trace_id`, `limit` | other | V31, V34 |

### 8.13 PromQL → TQS translation

| PromQL | TQS 3 |
| --- | --- |
| `sum by (a) (rate(m{l="v"}[5m]))` | `metric: m, match: [{l = v}], fn: {rate, 5m}, agg: {sum, by: [a]}` |
| `histogram_quantile(0.95, sum by (a, le) (rate(m_bucket[5m])))` | `metric: m, fn: {histogram_quantile, q: 0.95, window: 5m}, agg: {sum, by: [a]}` |
| `x / y` (same grouping) | two queries + formula `x / y` |
| `x * 3600`, `clamp_max(x, 1)`, `clamp_min(x, 1)` | formula `x * 3600`, `least(x, 1)`, `greatest(x, 1)` |
| `x or vector(0)` | `fill: "zero"` on a single-series query |
| `a or b * 0 or c * 0` | formula `coalesce(a, b * 0, c * 0)`, `join: "outer"` |
| `0 * absent(m{l="v"})` | query `agg: {op: absent}` + formula `z * 0` |
| `sum by (a) (max by (a, b) (m))` | `agg: {max, by: [a, b]}, then: {sum, by: [a]}` |
| `count(count by (a) (increase(m[$__range]) > 0))` | `agg: {sum, by: [a]}, having: {> 0}, then: {count}` |
| `label_replace(…, "dst", "$1", "src", "re")` | `derive: [{label: dst, from: src, regex: re, replacement: "$1"}]` |
| `x and on(k) topk(10, y)` | `y` as hidden query; `top: {n: 10, query: y}` |
| `time() - m` | formula `time - m` |
| `m offset 30m` | `offset: "30m"` |
| `x > 1` | `having: {op: ">", value: 1}` |

**Not supported in v1** [→ Q5, Q13](#questions): - subqueries `[range:step]` (1 of 99 real panels); - `@`; `unless`; - many-to-one matching (`group_left`/`group_right`), `on`/`ignoring` outside `and on(...) topk`; - `label_join`; - `predict_linear`, `deriv`, `holt_winters`, `quantile_over_time`, `stddev`/`stdvar`; - `count_values`, `quantile` across series, `sort`, `timestamp`, math functions beyond the formula set; - `bool` comparisons; - native-histogram queries.

`bottomk` = `top` with `order: "asc"` U.

### 8.14 Worked example: a real panel

PromQL on the Margince board (queue depth that keeps every declared queue visible at 0):

```promql
sum by (queue, kind) (max by (queue,kind,workspace_id) (margince_job_queue_depth{kubernetes_namespace="$namespace"}))
  or sum by (queue, kind) (margince_job_declared_info{kubernetes_namespace="$namespace",queue!=""} * 0)
  or sum by (queue, kind) (max_over_time(margince_job_queue_depth{kubernetes_namespace="$namespace"}[7d]) * 0)
```

TQS 3 (430 points equal to Prometheus R05):

```json
{ "tqs": "3", "id": "queue-depth", "kind": "timeseries", "time": {"from": "-6h", "step": "1m"},
  "vars": {"namespace": "ns-a"},
  "queries": [
    { "name": "a", "metric": "margince_job_queue_depth", "match": [{"label": "kubernetes_namespace", "op": "=", "value": "$namespace"}],
      "fn": {"op": "last"}, "agg": {"op": "max", "by": ["queue", "kind", "workspace_id"]}, "then": {"op": "sum", "by": ["queue", "kind"]} },
    { "name": "b", "metric": "margince_job_declared_info",
      "match": [{"label": "kubernetes_namespace", "op": "=", "value": "$namespace"}, {"label": "queue", "op": "!=", "value": ""}],
      "fn": {"op": "last"}, "agg": {"op": "sum", "by": ["queue", "kind"]} },
    { "name": "c", "metric": "margince_job_queue_depth", "match": [{"label": "kubernetes_namespace", "op": "=", "value": "$namespace"}],
      "fn": {"op": "max_over_time", "window": "7d"}, "agg": {"op": "sum", "by": ["queue", "kind"]} } ],
  "formulas": [ { "name": "depth", "expr": "coalesce(a, b * 0, c * 0)", "join": "outer" } ] }
```

---

## 9. Dashboards and alerts

- **Panel** = one TQS document + display settings (type, unit, legend template from label names). A **dashboard** = panels + variables with their allowed values (a variable's options come from a `table` TQS query over labels) U [→ Q12](#questions).
- **Grafana:** without PromQL, Grafana needs a TQS data source plugin, or we ship our own UI [→ Q11](#questions).
- **Alert rule** = TQS `table` (or `timeseries` with the last point), a condition (`having`-style op and threshold), `for` duration, labels and annotations. It is evaluated by the coordinator every interval, and alerts are sent to Alertmanager's v2 API or a webhook U [→ Q13](#questions).
- **No data:** a rule declares `on_no_data: ok | alerting | nodata`. `fill` and `absent` let a rule express "missing is zero" explicitly R04z, R01.

### 9.1 Materialized panels

Panels that are viewed often, and every alert rule, can be maintained incrementally E8: - input: rows of files from `ingest` envelopes E6, D5; - state: long-format, inlined `view_partials`; - quantiles from log buckets (scale 5); - read p50 ≈ 12.5 ms; - target p95 \< 50 ms.

The v5 "\< 1 ms" target is not reachable through a PostgreSQL catalog E8, P. Which TQS shapes can be materialized: - sums, counts, min/max, last and log-bucket quantiles can; - `rate`/`increase` with Prometheus extrapolation need the per-series first/last sample per bucket, as part of the partial state U.

---

## 10. AI agents (MCP)

Server: the `rmcp` crate, over stdio and streamable HTTP.

Tools: - `discover_telemetry_catalog`: metric names, label keys, a sample of label values, services and span names, all bounded; - `run_tqs`, with the TQS schema inlined in the tool definition; - `get_trace_waterfall`, `search_logs`; - `correlate_anomalies`: incident vs baseline share per dimension, lift × support with a χ² test, top 10 U; - `save_panel`: admin scope only, absent in `--read-only`.

Every result carries `rows_total`, `truncated`, `bytes_scanned`, `snapshot_id`, `freshness_s` and `partial_regions`. Telemetry text goes in a delimited data field, never in tool prose, which limits prompt injection from logs.

Validation errors return all problem codes with JSON paths (§8.12) V01–V36. The validator never silently drops a field V01.

---

## 11. Security and privacy

- **Auth:**
- bearer tokens map to `{tenant, scopes: ingest | query | admin}` on every API, MCP included;
- mTLS between nodes;
- laptop mode uses a read-only object-storage key scoped to a tenant prefix U.
- **Injection:**
- every query string is a bound parameter;
- label names and fields are validated against a fixed grammar;
- formulas are an AST A1, A2, V16, V22, V36.
- **Data:**
- ingest-time redaction of known secret attributes (`authorization`, `cookie`, `password`, `token`, `api_key`) U;
- erasure rewrites files then swaps them (§6.3) U.
- **Limits:**
- ≤ 10 queries, ≤ 11,000 points per series, regex ≤ 256 characters, formula ≤ 512 characters, `in` ≤ 100 values, `limit` ≤ 5,000;
- query memory, timeout and bytes scanned are configurable U.
- **License:** Apache-2.0, with dependency licences checked in CI.

---

## 12. Multi-region

- Ingest writes only to its region's bucket; one catalog references every bucket E3, E4.
- The PostgreSQL primary sits in one region with read replicas elsewhere.
- Each bucket gets its own scoped DuckDB secret U.
- Residency: an `in-region-only` region answers only federated aggregates, never raw rows U.
- Cross-region stitching of a trace needs nothing special E4.

---

## 13. Operability

- One YAML config, overridable by env vars; the effective config is printed at start with secrets redacted.
- Self-metrics: ingest rate, rejects by reason, flush latency, arena use, commit lag, compaction backlog, query latency, bytes scanned, cache hits.
- Format versioning: `format` in envelopes and `schema_version` in Parquet metadata. A binary refuses a newer major version and names both.
- Packaging: the image carries the `httpfs`, `ducklake` and `postgres_scanner` extensions for the bundled DuckDB version, with `autoinstall_known_extensions=false`. A DuckDB upgrade moves crate, extensions and catalog format together and is gated by `research/scripts/run-all.sh` P.

---

## 14. Edge cases

| Edge case | Behaviour | Evidence |
| --- | --- | --- |
| Counter reset inside a window | Added back (Prometheus correction) | M01–M03, MUT no_reset_correction |
| Series starts mid-range / stops | No point before the 2nd sample / none 5 min after the last | M01, M10 |
| Missing scrapes, jittered timestamps | Extrapolation as in Prometheus | M01–M04 |
| Window shorter than scrape interval | Empty result | M31 |
| Label value with a quote / non-ASCII text | Matched exactly / stored and searched | M28 / L4 |
| Empty label value | Same as absent; `!=""` filters it | R05, R10 |
| SQL-looking values | Bound as data | A1, A2 |
| Operand missing for some groups | `inner` drops the group; `fill`/`outer` keeps it | D1, R05 |
| Division by zero | No value | D2 |
| No data at all | Single-series `fill: zero` gives 0; `absent` gives 1 | \[R01, R02, R09 on `ns-empty`\], R04z |
| Quantile in the +Inf bucket | Highest finite bound (10 s in the fixtures) | M08 |
| Histogram reset | Corrected like a counter | M07, M08 |
| Lost parent span | Child shown at depth 0, flagged orphan | T3 |
| Trace across regions / flush boundary | One lookup, all spans | E4, T2 |
| Node crash (memory mode) | Unflushed batch lost; counter totals exact except resets inside the lost batch | E5 |
| Crash between catalog commit and envelope delete | No duplicates | E3 |
| Envelope with a missing file | Whole envelope rolled back | E3 |
| Compaction during view maintenance | Views ignore it (fed from envelopes) | E6 |
| Hot batch committed but still in RAM | Counted once | E7 |
| Retention of externally written files | DuckLake deletes them | E6 |
| Too many points / queries | Refused with code | V05, V08 |
| Unknown field, function or variable | Refused with code | V01, V21, V35 |

---

## 15. Implementation plan

### 15.1 Rust workspace

```
crates/core          envelope, schema (Arrow + DDL from one definition), series identity, log masking
crates/write         arena, durability modes, sorted batch builders, Parquet writer, flusher
crates/receivers     otlp (tonic), prom_remote_write (axum + prost + snap), loki_push
crates/catalog       DuckLake: apply envelopes, swap, watermarks, lease + fence, snapshot export
crates/coordinator   checkpointer, compactor, retention, materializer, alert evaluator, exporter
crates/tqs           model, validation, formula AST, compiler   ← start from research/src/tqs
crates/query         DuckDB pool, hot-tail client/server, caches, response envelope
crates/api           HTTP: TQS, dashboards, rules; Grafana data source endpoints (→ Q11)
crates/mcp           rmcp server
crates/bin           `blobobs` binary, roles
```

`research/src/tqs/` (model, validator, formula parser, compiler) and its suite are the seed of `crates/tqs`. The suite becomes the crate's integration test, with Prometheus as the reference and the mutation check kept.

### 15.2 Milestones

These assume 2–3 engineers; the estimates are ranges.

| Milestone | Scope | Exit test |
| --- | --- | --- |
| M1 (6–8 wk) single-node metrics | remote-write + OTLP metrics; `wal`; writer, flusher, envelopes; batched checkpointer; compactor; retention; `crates/tqs` from research; TQS HTTP API | wired test (§15.3) on the AI router and jobs panels; kill -9 loses no acked sample |
| M2 (6–8 wk) traces + logs + agents | OTLP traces/logs, Loki push; span compactor; `traces_index`; list kinds; MCP; laptop export; auth | T/L cases on real S3; MCP scenarios |
| M3 (4–6 wk) cluster | roles, lease + fence, hot-tail RPC, `replicated` | E7 across processes; partition tests |
| M4 (4–6 wk) dashboards + alerts | panel/dashboard store, variables, alert evaluator, materialized panels, Grafana plugin or UI (→ Q11) | every Margince panel rendered; alerts fire on fixtures |
| M5 (\~4 wk) multi-region | per-region buckets and secrets, catalog replicas, residency | cross-region traces and totals exact |

### 15.3 Wired testing with Margince metrics

Margince already serves Prometheus metrics on `/metrics` from the API and the worker: AI-call counters by provider/model/task/tier, a latency histogram with bounds 0.005–10 s, job and queue gauges per workspace, pool and HTTP metrics. Its Grafana board is recorded in margince-qc (`shared/stag-dashboards.csv`, 99 PromQL panels).

1. Run a local Margince stack (`make dev` in the margince checkout) and generate traffic (margince-qc `make seed-volume`, plus scenario walks).
2. Scrape both `/metrics` endpoints with one collector (OTel Collector or a Prometheus agent) that writes to **both** a reference Prometheus and the engine (remote-write).
3. Translate every board panel to TQS. The 11 shapes in R01–R11 are the templates; R-case translations are mechanical for 98/99 panels.
4. For each panel, compare TQS against Prometheus with the panel's own PromQL, step by step, as the research suite does. Report per panel: equal, divergent (documented: D1, D2), or not expressible (the 1 subquery panel → Q5).
5. Keep it as a nightly job against the pinned Margince version. The local stack is the default [→ Q20](#questions).

---

## 16. Questions for the product owner

Each question has a proposed default; nothing blocks M1 except Q7 and Q11.

| # | Question | Proposed default | Why it matters |
| --- | --- | --- | --- |
| Q1 | Should `rate`/`increase` keep Prometheus extrapolation (proven equal), or also offer an exact, non-extrapolated `increase_exact` for count-type stat panels ("calls today")? | Keep Prometheus semantics; add `increase_exact` in M4 if users ask | Extrapolated increases are not whole numbers and do not sum exactly across windows |
| Q2 | Division by zero: gap (no value) or Prometheus NaN/Inf? | Gap | NaN/Inf points break charts and alert math; D2 shows the difference |
| Q3 | Spans counted by start time or end time? | Start time (sorted column; matches Tempo) | Long spans shift by their duration between buckets |
| Q4 | Event bucket at `t` covers `(t − step, t]` (like metrics) or `[t, t + step)` (label at bucket start)? | `(t − step, t]` | Consistency with metrics vs the Grafana convention |
| Q5 | Subqueries (1/99 panels: jobs discarded/cancelled table): support them, or change that panel/metric to counters + `increase`? | Change the panel; no subqueries in v1 | Subqueries are the most complex PromQL feature |
| Q6 | Is "rewrite aggregation before the formula" acceptable, or should formulas take a post-aggregation? | Acceptable (1 panel) | Simpler engine; the translator rewrites |
| Q7 | Default durability: `wal` (single) / `replicated` (cluster), or `memory`? | `wal` / `replicated` | Sets the RPO users get |
| Q8 | Tenancy in v1: one tenant with the column in place, or isolation from M3? | One tenant | Isolation touches auth, limits and residency |
| Q9 | Product and binary name? | `blobobs` placeholder | Schema `$id`, bucket layout, docs |
| Q10 | Primary store first: S3 or R2? | S3 | Benchmarks and conditional-write details |
| Q11 | Grafana: build a TQS data source plugin, ship our own UI, or both? | Grafana plugin in M4 | Without PromQL, Grafana cannot query us otherwise |
| Q12 | Where do dashboards live: files in git (JSON/YAML) synced to the engine, or the engine's store with versions? | Files in git, loaded by the engine | Review and history for panels |
| Q13 | Alerting: our evaluator + Alertmanager webhook, or rules only as materialized panels watched by an external system? | Our evaluator + Alertmanager v2 API | Alert semantics, `for`, no-data |
| Q14 | Default retention: raw 15 d, 5 m rollups 90 d, 1 h rollups 13 months? | As written | Cost; 7 d `max_over_time` panels read raw data |
| Q15 | Provide Grafana's `$__rate_interval`? | No; panels use fixed windows (the real board does) | Needs a known scrape interval per series |
| Q16 | Empty label value = absent label (Prometheus rule)? | Yes | Affects `!=""` filters R10 |
| Q17 | Log masking rules are a fixed, versioned list (no per-tenant rules in v1)? | Yes | Stable `pattern_hash` across nodes |
| Q18 | Are the limits right: 10 queries, 11,000 points, regex 256, formula 512, `limit` 5,000? | Yes | Agent cost control |
| Q19 | Laptop mode: snapshot export (internals-coupled, 5 min fresh) or a read-only PostgreSQL role? | Snapshot export + CI gate | Network access vs coupling |
| Q20 | Wired test target: local Margince dev stack only, or also a shared environment (private repo only)? | Local stack | Public-repo rules forbid naming running installs |

Contents

1. [1. Purpose](#)
2. [2. Environment](#)
3. [3. Method](#)
4. [4. Results](#)
5. [5. Findings that changed the design](#)
6. [6. One-off probes \[P\]](#)
7. [7. What this research does not show](#)
8. [8. History](#)

# Research — testing and notes

This file records what was tested, how, what it showed, and what it did not show. Every number here comes from a run of `research/scripts/run-all.sh` on 2026-11-10. The raw outputs are in `research/results/`.

## 1. Purpose

Turn the v5 specification's ideas into facts before implementation: 1. Does the storage design work on S3-compatible storage with DuckLake? (experiments E1–E9) 2. Can our own query schema (TQS 3) give dashboards the same numbers as PromQL? (cases M, D, R) 3. Do spans, logs and traces queries return what an independent computation says? (cases S, L, T) 4. Does the schema refuse what it must, and treat hostile input as data? (cases V, A) 5. Can the test suite fail? (mutation check) 6. How much of a real dashboard does TQS cover? (the Margince Grafana board)

## 2. Environment

| Component | Version / setup |
| --- | --- |
| Language | Rust 1.97, edition 2024 |
| Query engine | DuckDB 2.0.0 via the `duckdb` crate 1.20000.1 (bundled), extensions `ducklake`, `httpfs`, `postgres_scanner` 2.0.0 |
| Arrow / Parquet / S3 client | `arrow` 60, `parquet` 60, `object_store` 0.14 |
| Object storage | MinIO (built from the Go module proxy), local, two buckets `telemetry-us-east`, `telemetry-eu-west` |
| Catalog | DuckLake on PostgreSQL 16 (private cluster, port 55432) |
| Reference | Prometheus 3.16 (module v0.316.0) and promtool, built from the Go module proxy |
| Machine | 4 vCPU, 15 GB RAM, local disk; MinIO and PostgreSQL on the same host |

The DuckDB extensions came from the PyPI wheels `duckdb-extension-*==2.0.0`: the public extension host was not reachable from this machine. That is the reason for decision D12 (ship extensions with the binary).

**Rerun:**

```bash
cd research
GOBIN=$PWD/.tools go install github.com/minio/minio@latest
GOBIN=$PWD/.tools go install github.com/prometheus/prometheus/cmd/prometheus@latest github.com/prometheus/prometheus/cmd/promtool@latest
# DuckDB extensions: see research/README.md
MARGINCE_QC=../../margince-qc scripts/run-all.sh   # last argument optional: real-board coverage
```

## 3. Method

| Evidence kind | Oracle | Why it is trustworthy |
| --- | --- | --- |
| Storage experiments (E) | Ground truth kept by the generator: every span and sample, including the ones a simulated crash "lost" | The generator knows what reached storage and what did not |
| Metric cases (M, D, R) | Prometheus 3.16 on the **same** samples, imported with `promtool tsdb create-blocks-from openmetrics`; compared point by point at every step | Prometheus is the reference implementation of the semantics dashboards rely on |
| Event cases (S, L, T) | A separate Rust computation over the generated spans and logs (loops, R-7 quantile) | Independent code path from the SQL compiler |
| Validation (V, A) | Expected problem code per case; SQL-looking values must return 0 rows and leave tables intact |  |
| Mutation check | 15 deliberate breaks of one rule each in the compiler | A suite that cannot fail proves nothing |
| Real-board coverage | The 99 PromQL panels margince-qc records for the Margince Grafana board | Real use, not our own choice of functions |

**Equality rule.** For every (series key, step) both sides must have a point, and the values must agree within 1e-9 relative. The largest difference seen was 2.9e-14, from floating-point summation order. A point present on one side only counts as a failure.

**Fixtures.** They are deterministic, seeded, and shaped like Margince's own metrics: - AI-call counters by provider/model/task/tier/outcome; - a latency histogram with Margince's bounds (0.005–10 s, +Inf); - queue and pool gauges; - the board's labels (`kubernetes_namespace`, per-workspace gauges, `declared_info`, routes, cAdvisor containers); - 349 series, 161,154 samples, 14,999 spans, 4,288 logs.

Edge cases are planted on purpose and named in `research/src/fixtures.rs`: - a counter reset, a late-starting series, a stopping series; - 3 % missing scrapes, 0–2 s timestamp jitter; - a label value with a quote, a provider with no error series, a flat-zero counter; - a histogram whose p99 sits in the +Inf bucket, and a histogram reset; - a namespace with no error series and one with no data at all; - a trace with a lost parent span; - non-ASCII log text.

## 4. Results

### 4.1 Summary

| Area | Result |
| --- | --- |
| Storage experiments E1–E9 (MinIO + DuckLake) | **9/9 pass** |
| Same TQS queries on DuckLake tables in MinIO vs in memory | **6/6 identical** (up to 667 points each) |
| Metric cases vs Prometheus (M01–M33) | **33/33 equal** |
| Deliberate divergences (D1–D2) | **2/2 behave as specified** |
| Real-board panel shapes vs Prometheus (R01–R11, 17 runs over 3 namespaces) | **17/17 equal** |
| Points compared with Prometheus, total | **30,758, all equal** |
| Spans / logs / traces vs Rust (S1–S6, L1–L5, T1–T3) | **14/14 equal** |
| Validation and injection (V01–V36, A1–A2) | **38/38** |
| Mutation check | **15/15 mutants caught** |
| Real Margince board, construct level | **98/99 panels expressible** (the 1 exception uses a subquery) |

### 4.2 Storage experiments

| ID | Claim | Key numbers |
| --- | --- | --- |
| E1 | MinIO gives If-None-Match / If-Match | second create refused; stale ETag → 412; 1 of 8 racing lease writers won |
| E2 | Ingest needs no DuckDB, no routing | 200,000 spans + 144,000 samples generated, 44 files + 11 envelopes in ≈ 0.77 s; a crash lost 21,133 spans / 12,063 samples (its unflushed batch) |
| E3 | One catalog, two buckets, atomic and idempotent | commit p50 ≈ 41 ms (4 files, PostgreSQL); replay after a crash skipped 1, no duplicates; an envelope with a missing file rolled back fully |
| E4 | Trace lookup across regions and flush boundary | 5/5 spans, 2 regions; chosen layout 26 GETs / 294.7 KiB; no bloom 22 / 623.8 KiB; v5 layout 38 / 509.6 KiB; absent id 22 GETs = 2 per file |
| E5 | Counter increase without ingest state | read-time and compactor paths: 0 series off the best possible; the v5 ingest-state rule lost **9.33 %**; L1 4.60 B/sample, L0 6.18 B/sample |
| E6 | Own compactor; change feed; file ownership | 178,867 rows before and after a swap of 11 → 2 files; lookup 26 → 6 GETs; change feed reported 178,867 inserts + 178,867 deletes; DuckLake deleted 11/11 external L0 files |
| E7 | Hot tail with a per-node watermark | correct before flush, after commit, after drop; naive union double-counted 60,000; Arrow scan of 60k rows ≈ 120 ms |
| E8 | Mergeable percentiles, inlined partials | worst error 1.01 % (p50/p95/p99, scale 5); 1,339 partial rows inlined, 0 files; panel read p50 ≈ 12 ms |
| E9 | Laptop mode from an S3 catalog snapshot | attach ≈ 45 ms, lookup ≈ 28 ms, write refused; **requires flushing inlined rows first** |

### 4.3 Query schema (TQS 3)

The full per-case table is in `research/results/tqs-coverage.md`. Highlights: - **Prometheus semantics reproduced exactly:** - `rate`, `increase`, `irate`, `delta` with extrapolation and reset correction; - the `*_over_time` family, `resets`, `changes`; - `last` with the 5 min lookback, including a stale series; - `histogram_quantile` with bucket interpolation and the +Inf edge; - aggregation with `by`/`without`, `count` of series, per-step `topk`, `offset`, regex and negative-regex matchers, value filters, scalar arithmetic, clamps, formulas over formulas. - **Real panels.** Each was checked with the panel's own PromQL, after `$namespace` and `$__range` substitution: - stat with `or vector(0)`; - error % with `clamp_min`/`clamp_max`; - histogram p95 over the range; - calls per hour + the `0 * absent(…)` line; - queue depth with `or … * 0` baselines and two-level aggregation; - uptime from `time()`; - p95 of the 10 busiest routes (`and on(route) topk`); - active-model count (`count(count by … > 0)`); - CPU by pod with a `label_replace`-derived role; - declared job count.

The namespace without error series and the namespace with no data give the same 0 / empty result as Prometheus. - **Deliberate divergences:** - D1: `fill: zero` keeps a group that PromQL drops when one operand has no series; - D2: x/0 is a gap where Prometheus returns NaN. - **Latency** (small fixtures, in memory): 5–60 ms per query. The same queries on DuckLake in MinIO took 28–127 ms.

### 4.4 Mutation check

| Mutant (rule broken) | Cases that turned red |
| --- | --- |
| no reset correction | 16 |
| closed window `[t−w, t]` | 29 |
| no extrapolation | 22 |
| lookback 4 m instead of 5 m | 3 |
| histogram returns bucket upper bound | 4 |
| `without` ignored | 1 |
| `fill` ignored | 6 |
| x/0 gives ±Inf/NaN | 1 |
| event buckets include their left edge | 1 |
| `derive` ignored | 1 |
| `then` ignored | 2 |
| outer join behaves as inner | 1 |
| `top.query` ignored | 1 |
| `absent` without labels | 1 |
| `time` in ms | 1 |

Seven rules are guarded by a single case. They are listed so that a refactor does not remove that one case.

### 4.5 Real board coverage

`research/scripts/dashboard_coverage.py` reads margince-qc's record of the board and writes only aggregate counts (`research/results/dashboard-coverage.md`). It reports 98/99 panels expressible and 1 needing a subquery. Constructs by panel count:

| Construct | Panels |
| --- | --- |
| `sum` | 71 |
| `rate` | 41 |
| `or` | 32 |
| `max` | 31 |
| `vector(0)` | 28 |
| `absent` | 16 |
| `increase` | 10 |
| `histogram_quantile` | 8 |

Every construct has a TQS equivalent and at least one verifying case.

Construct-level coverage is not numeric proof for each panel. Numeric proof exists for the 11 shapes R01–R11. The wired test in SPECIFICATION §15.3 extends it to every panel on a live Margince scrape.

## 5. Findings that changed the design

1. **Counter deltas belong at read/compaction time, not ingest** (E5). The v5 rule loses 9.33 % in one crash; the read-time path loses nothing beyond physically lost resets.
2. **DuckLake's change feed cannot drive views** (E6). A compaction shows as N inserts + N deletes.
3. **DuckLake deletes files it did not write** once they expire (E6). Retention can go through it; double registration is forbidden.
4. **DuckDB's Parquet writer writes no bloom filter on high-cardinality columns** P. Span files need our writer.
5. **t-digest state is not storable** P. Log buckets are, and they merge with SUM within 1.01 % (E8).
6. **Laptop snapshot export must flush inlined rows first** (E9). The first attempt failed on PostgreSQL's backend-specific types for inlined rows.
7. **Trace-lookup cost is linear in file count** (E4, E6). Hourly compaction and time-bounded lookups are required.
8. **"\< 1 ms" dashboard reads are not possible through a networked catalog** (E8, P ≈ 20 ms for 10 inlined rows). The target is now p95 \< 50 ms.
9. **PromQL's dashboard features are reproducible in a closed JSON schema with equal numbers** (M, R). The real board needs variables, outer joins (`or`), `absent`, two-level aggregation, `label_replace` and `and on … topk`. These were added as TQS constructs after reading the real panels, then verified.
10. **DuckDB 2.0 rejects the old `->` lambda syntax** P. The compiler uses `lambda x: …`.

## 6. One-off probes P

| Probe | Observation |
| --- | --- |
| DuckLake functions in 2.0.0 | 21 `ducklake_*` functions, including `add_data_files`, `rewrite_data_files`, `merge_adjacent_files`, `flush_inlined_data`, the change feed |
| `add_data_files` | registers files outside the data path; records per-file min/max |
| `DELETE … WHERE filename IN (…)` + `add_data_files` in one transaction | atomic swap; whole-file delete writes no delete file |
| `merge_adjacent_files` | not reported as inserts by the change feed, but rewrites into DuckLake-named files without our sort/bloom |
| Data inlining on PostgreSQL | small inserts write no file; reading 10 inlined rows ≈ 20 ms |
| Concurrent writers (4 × 10 inserts, one table) | 0 errors, p50 16 ms, worst 972 ms |
| DuckDB HTTP logging | counts every S3 range GET per query (used by E4) |
| `approx_quantile … EXPORT_STATE` | works in process; after Parquet it reads back as a plain STRUCT that `combine()` rejects |
| DuckDB `1/0`, `0/0` | `inf`, `nan` (IEEE, like Prometheus) |

## 7. What this research does not show

| Not shown | Why it matters | Where it is planned |
| --- | --- | --- |
| Performance at production scale and on real S3 latency | MinIO on the same host has \~ms latency; fixtures are small | M1 benchmark, SPEC §15.2 |
| Durability modes other than `memory` | `wal`/`replicated` are designs only | M1 / M3 |
| Hot tail across processes | Proven in one process | M3 |
| Compaction swap racing a concurrent add | Untested | single-writer coordinator; add a test |
| `events`/`links` list-of-struct columns via `add_data_files` | Untested | first sprint |
| Native/exponential histogram queries | Only classic buckets are proven | Q-list / later |
| Materialized panels with Prometheus-extrapolated `rate` | Needs per-bucket first/last samples | M4 |
| Every one of the 99 real panels numerically | 11 shapes proven; 98 expressible | wired test, SPEC §15.3 |
| Bytes per sample on real data | Synthetic random walks | M1 with real exporter data |
| `top` ranks other than `per_step`; `bottomk` | Untested | first sprint |

## 8. History

The earlier documents are kept in `history/` for traceability: - `v5-review.md`: the review of the v5 specification that started this work; - `concept-v1.md`: the first PoC concept, with PromQL as a planned interface.

SPECIFICATION.md supersedes both where they disagree. The main changes are: no PromQL, TQS 3 as the only language, and these research results.

# Questions for the product owner

Each has a proposed default. Answer by number; nothing blocks milestone 1 except Q7 (durability default) and Q11 (Grafana plugin or own UI).

1. Q1

   Should `rate`/`increase` keep Prometheus extrapolation (proven equal), or also offer an exact, non-extrapolated `increase_exact` for count-type stat panels ("calls today")?

   Proposed default

   Keep Prometheus semantics; add `increase_exact` in M4 if users ask

   Why it matters

   Extrapolated increases are not whole numbers and do not sum exactly across windows
2. Q2

   Division by zero: gap (no value) or Prometheus NaN/Inf?

   Proposed default

   Gap

   Why it matters

   NaN/Inf points break charts and alert math; \[D2\] shows the difference
3. Q3

   Spans counted by start time or end time?

   Proposed default

   Start time (sorted column; matches Tempo)

   Why it matters

   Long spans shift by their duration between buckets
4. Q4

   Event bucket at `t` covers `(t − step, t]` (like metrics) or `[t, t + step)` (label at bucket start)?

   Proposed default

   `(t − step, t]`

   Why it matters

   Consistency with metrics vs the Grafana convention
5. Q5

   Subqueries (1/99 panels: jobs discarded/cancelled table): support them, or change that panel/metric to counters + `increase`?

   Proposed default

   Change the panel; no subqueries in v1

   Why it matters

   Subqueries are the most complex PromQL feature
6. Q6

   Is "rewrite aggregation before the formula" acceptable, or should formulas take a post-aggregation?

   Proposed default

   Acceptable (1 panel)

   Why it matters

   Simpler engine; the translator rewrites
7. Q7

   Default durability: `wal` (single) / `replicated` (cluster), or `memory`?

   Proposed default

   `wal` / `replicated`

   Why it matters

   Sets the RPO users get
8. Q8

   Tenancy in v1: one tenant with the column in place, or isolation from M3?

   Proposed default

   One tenant

   Why it matters

   Isolation touches auth, limits and residency
9. Q9

   Product and binary name?

   Proposed default

   `blobobs` placeholder

   Why it matters

   Schema `$id`, bucket layout, docs
10. Q10

    Primary store first: S3 or R2?

    Proposed default

    S3

    Why it matters

    Benchmarks and conditional-write details
11. Q11

    Grafana: build a TQS data source plugin, ship our own UI, or both?

    Proposed default

    Grafana plugin in M4

    Why it matters

    Without PromQL, Grafana cannot query us otherwise
12. Q12

    Where do dashboards live: files in git (JSON/YAML) synced to the engine, or the engine's store with versions?

    Proposed default

    Files in git, loaded by the engine

    Why it matters

    Review and history for panels
13. Q13

    Alerting: our evaluator + Alertmanager webhook, or rules only as materialized panels watched by an external system?

    Proposed default

    Our evaluator + Alertmanager v2 API

    Why it matters

    Alert semantics, `for`, no-data
14. Q14

    Default retention: raw 15 d, 5 m rollups 90 d, 1 h rollups 13 months?

    Proposed default

    As written

    Why it matters

    Cost; 7 d `max_over_time` panels read raw data
15. Q15

    Provide Grafana's `$__rate_interval`?

    Proposed default

    No; panels use fixed windows (the real board does)

    Why it matters

    Needs a known scrape interval per series
16. Q16

    Empty label value = absent label (Prometheus rule)?

    Proposed default

    Yes

    Why it matters

    Affects `!=""` filters \[R10\]
17. Q17

    Log masking rules are a fixed, versioned list (no per-tenant rules in v1)?

    Proposed default

    Yes

    Why it matters

    Stable `pattern_hash` across nodes
18. Q18

    Are the limits right: 10 queries, 11,000 points, regex 256, formula 512, `limit` 5,000?

    Proposed default

    Yes

    Why it matters

    Agent cost control
19. Q19

    Laptop mode: snapshot export (internals-coupled, 5 min fresh) or a read-only PostgreSQL role?

    Proposed default

    Snapshot export + CI gate

    Why it matters

    Network access vs coupling
20. Q20

    Wired test target: local Margince dev stack only, or also a shared environment (private repo only)?

    Proposed default

    Local stack

    Why it matters

    Public-repo rules forbid naming running installs

# Download

The zip is the whole research folder: specification, research notes, the Rust research code with its results, and the superseded documents. `research/scripts/run-all.sh` reproduces every number.

**observability-engine-research.zip**176 KB · 29 files

**SPECIFICATION.md**47 KB · the product specification

**RESEARCH.md**13 KB · testing and notes

## Folder

```
observability-engine-research/
  README.md            index and results at a glance
  SPECIFICATION.md     the product specification (final for implementation start)
  RESEARCH.md          method, results, findings, limits
  history/             v5 original spec, its review, concept v1
  research/            Rust crate: src/, scripts/, results/
    src/tqs/           TQS 3 model, validator, formula AST, compiler
    src/suite.rs       cases vs Prometheus, Rust oracles, mutation check
    scripts/run-all.sh reproduce everything from a clean stack
    results/           poc-results, tqs-coverage, tqs-lake, dashboard-coverage
```