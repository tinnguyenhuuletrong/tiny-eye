# Blob-First AI Observability Engine — v5 Review and v6 Completion

**Input:** *Master Architecture & Product Specification v5.0* (744 lines).
**Output of this document:** what must change before implementation starts, the sections v5 is
missing, the decisions the team must take, and a spike plan that proves the risky assumptions first.

**How to use it:** v5 stays the base text. Every section below either *replaces* a v5 section (marked
**Replaces §x**) or *adds* one (marked **New**). Sections of v5 not named here stand as written.

---

## 0. Verdict

The direction is sound: object storage as the primary tier, sorted ZSTD Parquet, an embedded DuckDB
query engine, a declarative query spec for agents, and an MCP server. That combination is real and
worth building.

The v5 text is **not ready to implement**. It has:

- **13 blocking defects** (§2) — places where the design as written loses data, double-counts,
  cannot be built with the named tool, or is unsafe.
- **11 factual claims that are wrong or unproven** (§3), several of them headline numbers.
- **Whole areas with no design at all**: durability/ack contract, security and tenancy, alerting,
  Grafana *read* compatibility for logs and traces, query cost limits, format versioning.
- **A roadmap that is roughly 5–8× too short** for the scope (§9).

The single most valuable change (D2 in §4): **move counter-delta computation out of the ingest path.**
It removes consistent-hash routing, per-node series state, state hydration and handoff, and it makes
the "kill -9 with 0% counter error" test achievable. Ingest becomes genuinely stateless.

### 0.1 What was verified, and what was not

Verified by running DuckDB **2.0.0** locally (scripts in the session scratchpad, results quoted
where used):

| Claim checked | Result |
| :--- | :--- |
| DuckDB's Parquet writer gives `trace_id` a bloom filter | **No.** On 200k rows / 4 row groups, a bloom filter was written for dictionary-encoded `service_name` only; none for `trace_id` stored as `UUID` or as `BLOB`. |
| Sorting by `trace_id` gives prunable row-group stats | **Yes.** Row-group min/max ranges were disjoint (`00004cb3…–545b4da0…`, `545b90d7…–a8158e30…`, …). |
| "Serialized DuckDB T-Digest state" can be stored and merged | **No, not durably.** `approx_quantile(...) EXPORT_STATE` works in-process, but once written to Parquet it reads back as a plain `STRUCT(count, min, max, centroids[])` and `combine()` rejects it. The internal state type is not a storage format. |
| A log-bucket histogram is mergeable in plain SQL | **Yes.** Scale-3 buckets (`ceil(log2(x)*8)`) merged by `SUM` gave p95 = 980.6 vs exact 949.4 (3.3 % error, inside the 9 % bucket width). |
| The §6.3 `ASOF JOIN` SQL runs | **Yes, it parses and runs** — but it has no time predicate on the points table (see B8). |

**Not verified:** every DuckLake-specific behaviour (`ducklake_add_data_files`, data inlining on a
PostgreSQL catalog, change-feed behaviour across compaction, multi-bucket paths). The DuckDB extension
host and the DuckLake docs site are blocked by this session's network policy. These are the first
spike (S1, §8) and they gate decision D3.

---

## 1. Severity legend

- **BLOCKER** — implementing as written loses data, produces wrong numbers, or is unsafe.
- **MAJOR** — works, but contradicts a stated target or makes a later phase expensive to change.
- **MINOR** — wording, a wrong number, a missing field.

---

## 2. Blocking defects

### B1 — No durability contract; acknowledged data is lost on crash (BLOCKER)
v5 buffers up to 60 s in RAM and flushes to S3. There is no WAL and no replication. A client that got
`200 OK` loses that data when the pod is OOM-killed, evicted or `kill -9`'d. Prometheus and the OTel
Collector stop retrying once they get a 2xx. §10.1 Test 1 ("`kill -9` … verify 0 % error") cannot
pass for gauges, spans or logs.
**Fix:** §5.1 — an explicit ack contract with selectable durability modes and a stated RPO per mode.

### B2 — Ingest-time `positive_delta` is wrong under rebalancing, crash and HA (BLOCKER)
- On scale-up/down or crash, series ownership moves. The new owner has no `series_last_point`, so it
  either drops one interval or records a false reset (a huge fake increase).
- During ring convergence two nodes can own the same series; both compute deltas against their own
  last point and the increase is **counted twice**.
- §5.1 says state is "hydrated in < 50 ms from the DuckLake catalog checkpoint". The catalog does not
  hold last sample values; those live in Parquet.
- First-sample, reset, OTel `start_time`, delta temporality, out-of-order and staleness-marker rules
  are not defined.
- HA Prometheus pairs carry **different** `replica`/`prometheus_replica` external labels, so they hash
  to **different** `series_id`s. The "(series_id, timestamp_ms) dedupe" in failure mode #19 never
  fires; every series is stored and summed twice.
**Fix:** D2 + §5.3 — deltas computed in exactly one place per partition; full counter semantics table;
an HA replica rule.

### B3 — Jump Consistent Hash cannot remove an arbitrary node (BLOCKER if routing is kept)
Jump hash maps keys to buckets `0..N-1` and only supports removing the *last* bucket. A stateless pod
fleet loses arbitrary pods. v5 also names no membership protocol.
**Fix:** if routing is kept (D2 makes it optional), use rendezvous (HRW) hashing over a gossip or
Kubernetes EndpointSlice member list — §5.4.

### B4 — Percentile state has no durable mergeable format (BLOCKER)
`tdigest_sketch BLOB` ("serialized DuckDB T-Digest state") does not exist as a storage format (§0.1).
Every `p50/p90/p95/p99` in rollups, histograms and materialized views depends on it.
**Fix:** log-bucketed histograms (OTel exponential-histogram layout) stored as `MAP(SMALLINT, UBIGINT)`
— mergeable with `SUM` per bucket in plain SQL, readable by any Parquet tool, and the same encoding
OTel and Prometheus native histograms already use. §5.7, §5.8.

### B5 — Materializer double-counts and does not scale as specified (BLOCKER)
- Compaction (L0→L1) writes the same rows into new files. If `ducklake_table_insertions()` reports
  rows of a compaction snapshot as inserts (unverified — S1), every view counts that hour twice.
  Retention deletes are never retracted from views either.
- One DuckLake table per panel (`lake.views.<panel_id>`) × 1,000 panels × a commit every 5 s = ~200
  catalog commits/s and ~17k snapshots/day/panel.
- `data_inlining_row_limit = 10000` is exceeded by one 1-minute panel over 7 days × 3 regions × a few
  groups (≈ 30k+ rows), so "zero S3 calls" does not hold.
- "< 1 ms" reads are not reachable through a PostgreSQL catalog round trip.
**Fix:** the materializer consumes **our own commit log** (`kind = ingest` envelopes only), keeps one
generic long-format partial table, and has its own TTL. Target p95 < 50 ms. §5.8.

### B6 — Trace layout contradicts its own rationale (BLOCKER for the trace-lookup target)
`traces_spans` is sorted `(region, service_name, trace_id, start_time_ns)`. A trace that touches five
services in one region sits in five separate places in the file, so "1 HTTP range request fetches the
entire regional trace segment" is false. And DuckDB's writer does not write a `trace_id` bloom filter
(§0.1), so a compactor built on `COPY … TO parquet` silently drops the bloom filters L0 files had.
**Fix:** sort spans by `(trace_id, start_time_ns)`; rely on disjoint row-group stats; write every file
(L0 and L1) with our own Parquet writer. §5.7.

### B7 — TQS `formulas[].expr` is free text compiled into SQL (BLOCKER — security)
An agent-supplied string pasted into SQL is SQL injection, and it reintroduces the hallucination
problem TQS exists to remove. Filter keys (`span_attrs.*`, `labels.*`) have the same problem.
**Fix:** a closed expression grammar parsed to an AST; filter keys validated against a per-signal
column list; all values bound as parameters. §5.9.

### B8 — The hand-written PromQL pipeline will not reach compatibility (BLOCKER for "drop-in")
- `increase()`/`rate()` in Prometheus **extrapolate** to the window edges; summing `positive_delta`
  does not. Shadow validation at "< 1 %" fails on low-rate counters.
- Staleness markers (the special NaN Prometheus sends when a target disappears) are not handled.
- The §6.3 SQL has no time predicate on `metrics_points_raw`, and `CROSS JOIN` steps × series has no
  limit (11k steps × 50k series = 550M rows).
- Five stages cover selectors, `rate`, `sum by`. Real dashboards use vector matching
  (`on/ignoring/group_left`), `histogram_quantile`, `offset`, `@`, subqueries, `topk`, `absent`,
  `label_replace`, `predict_linear`…
**Fix:** embed the upstream Prometheus PromQL engine (Go) behind a `storage.Queryable` backed by DuckDB,
then push down the hot shapes. Correctness comes from reuse; the Prometheus compliance tester measures
it. §5.10.

### B9 — No security, tenancy or privacy model (BLOCKER)
No authN/authZ on ingest, query, MCP or the public `/v1/rum` endpoint. Laptop MCP mode needs read
credentials to the whole production bucket. Telemetry carries PII and secrets; agents read it. There
is no erasure path for immutable Parquet. v5 also proposes importing Grafana components, and Loki,
Tempo, Mimir and Grafana are AGPL-3.0 — incompatible with an Apache-2.0/MIT target.
**Fix:** §5.15.

### B10 — "Global bucket" contradicts "zero cross-region WAN writes" (MAJOR → BLOCKER for residency)
S3 buckets are regional. Every region PUT-ing into one bucket *is* a cross-region write, with egress
cost. It also breaks EU data residency.
**Fix:** one bucket per region (or one R2 bucket with jurisdiction settings), one catalog that holds
absolute paths across buckets (verify in S1), residency rules on the global read lane. §5.11.

### B11 — In-memory 1 h rollups (MAJOR)
Holding an hour of rollup state per series in ingest RAM loses the hour on any crash and cannot absorb
late data. "Zero S3 read-back" saves very little: one hourly read of that hour's L0 files.
**Fix:** the compactor produces rollups from L1 after the hour closes plus a grace period. §5.14.

### B12 — S3 lifecycle TTL deletes files the catalog still references (MAJOR)
Queries then fail with 404 on files the catalog lists.
**Fix:** expire through the catalog first, delete files second; lifecycle rules only as a backstop set
longer than the catalog TTL. §5.14.

### B13 — Drain3 `pattern_hash` is not stable across nodes (MAJOR)
Drain learns templates from the stream each node sees. Two nodes, two regions or two restarts give the
same line different templates and hashes, so `correlate_anomalies` ("new pattern appeared") fires on
noise.
**Fix:** `pattern_hash` from **deterministic masking** (fixed regex tokenizer, versioned); Drain-style
clustering is an optional read-time grouping over those hashes. §5.7.

---

## 3. Corrected factual claims

| v5 claim | Correction |
| :--- | :--- |
| S3 at "~$0.015/GB-month" | S3 Standard is ~$0.023/GB-month; $0.015 is Cloudflare R2. Quote both. |
| "< 1.1 bytes/sample" | Unproven. Prometheus TSDB's Gorilla encoding is ~1–2 B/sample; Parquet with two `DOUBLE` columns (`value`, `positive_delta`) is unlikely to match. Make it a benchmark (S2) with a target, not a claim. |
| `series_last_point` "16 MB for 1M series" | 24 B payload per entry; real Go/Rust map overhead puts it at ~40–60 MB. Moot under D2. |
| RoaringBitmap series cache | Standard Roaring is 32-bit; `series_id` is 64-bit. Use Roaring64 / a treemap. |
| "1 HTTP range request per regional trace" | False under the v5 sort order (B6). |
| "Serialized DuckDB T-Digest" | Not a durable format (§0.1, B4). |
| "`JumpConsistentHash`" | Wrong algorithm for an elastic fleet (B3). |
| Dashboard reads "< 1 ms" | Not reachable through a networked catalog. Target p95 < 50 ms server-side (B5). |
| "Zero-disk stateless" + "spill to `/tmp` at 75 %" | `/tmp` is a disk. Say "ephemeral disk only". |
| Shadow validation "< 1 % Mean Squared Error" | MSE is not a percentage. Use per-point relative error, e.g. p99 relative error < 1 % over aligned steps. |
| "Phase 1–4 in 6 weeks" | See §9: roughly 6–9 months for 2–3 engineers. |

Smaller spec bugs:
- TQS Example 1 uses `"gte": "500"` — the schema types `gte` as a number. Also, a label value is a
  string, so a numeric comparison needs an explicit cast rule (§5.9).
- `metric_type` has no **Summary** (OTel `Summary`, Prometheus `summary`).
- `metrics_histograms_raw` lacks `negative_buckets`, bucket offsets, `zero_threshold` and the
  counter-reset hint that native histograms carry; histogram buckets are cumulative counters and
  need the same delta/reset rules as scalar counters.
- `$id`/`$ref` point to `fanout-lake.dev` — a third-party-sounding placeholder; MCP clients will not
  resolve remote `$ref` anyway. Inline the schema.
- `save_materialized_dashboard_panel` is a write tool; it must not exist in `--read-only` mode.
- `instance_id` in the `series_id` formula is undefined. If it means the *ingesting node*, the same
  series changes identity whenever routing changes. It must come only from the series' own labels.

---

## 4. Decisions

Recorded so implementation does not reopen them. "Proposed" means the team still has to sign off.

| # | Decision | Status | Why |
| :--- | :--- | :--- | :--- |
| D1 | **Language: Go.** | Proposed | Reuse decides schedule: the Prometheus PromQL engine, scrape + service discovery, remote-write protos, OTel `pdata` receivers, `memberlist`, `parquet-go` (writes split-block bloom filters), `duckdb-go` with Arrow. Rust wins on GC, but every reused piece above would be a rewrite. Use `GOMEMLIMIT` + arena accounting for the memory ceiling. |
| D2 | **Counter deltas are computed in exactly one place per partition, never at ingest.** Live data: the elected materializer (single writer per shard) keeps last-point state. Durable data: the compactor recomputes `positive_delta` into L1 from `value`, using the previous hour's last point. Raw L0 queries compute `value - lag(value)` at read time. | Proposed | Removes routing, per-node state, hydration and handoff (B2, B3). Cumulative counters then survive any ingest crash: the next sample's delta covers the lost interval, so totals stay exact. |
| D3 | **Catalog: DuckLake if S1 passes, else our own manifest.** Option A: DuckLake on PostgreSQL (cluster) / DuckDB file synced to S3 (single node). Option B: our own `files`, `file_column_stats`, `snapshots` tables in PostgreSQL/SQLite, DuckDB reads with `read_parquet([...])` after our pruning. | Gated on S1 | Our design bypasses DuckLake on write, compaction and change feed. If it cannot register external files with stats and replace files atomically, it adds coupling and little else. |
| D4 | **Durability default:** single node `wal` (local disk, replayed on restart); cluster `replicated` (RF=2 in-region). `memory` stays an explicit opt-in. | Proposed — team | §5.1. The RPO users accept decides this, not engineering. |
| D5 | **Tenancy:** a `tenant` column and an `X-Scope-OrgID`-style header from day 1; single tenant `default` in v1. | Proposed | Adding a partition key to every table and path later is a format migration. |
| D6 | **Routing:** none for correctness. Optional HRW affinity for metrics (compression only), behind a flag, after M3. | Proposed | Follows from D2. |
| D7 | **PromQL:** upstream engine + DuckDB `Queryable`; push down later. | Proposed | B8. |
| D8 | **Percentiles:** log-bucket histograms, default scale 5 (≤ ~1.1 % relative error), configurable. | Proposed | B4. |
| D9 | **License: Apache-2.0.** No AGPL code imported; each Grafana module checked one by one (some proto submodules are Apache-2.0, the servers are AGPL-3.0). | Proposed — team | B9. |
| D10 | **Out of v1.0:** RUM, sourcemaps, embedded web UI, AI Grafana migrator. Grafana is the UI for v1. | Proposed — team | Scope (§9). |

---

## 5. Completed and replaced sections

### 5.1 Durability and acknowledgement contract — **New**

A request is acknowledged only when the batch meets the configured mode:

| Mode | Ack after | Lost on single-node crash | Cost |
| :--- | :--- | :--- | :--- |
| `memory` | Accepted into the arena | Everything not yet flushed (≤ flush interval). Cumulative counter **totals** survive (D2); gauges, spans, logs, delta-temporality counters in that window do not. | None |
| `wal` | Group-committed `fsync` to local disk every 100–250 ms | Nothing, if the pod restarts on the same disk. Node loss = `memory`. | Local disk, a few ms of p99 |
| `replicated` | Batch held in RAM by 2 in-region peers | Needs 2 failures within one flush interval | 2× in-region ingest traffic |
| `s3-wal` | Small segment PUT to object storage (S3 Express One Zone where available), group commit ≤ 1 s | Nothing | +50–200 ms p99; ~86k PUT/day/node at 1/s |

Rules:
- `429` / gRPC `RESOURCE_EXHAUSTED` only when the client is expected to retry; never drop silently.
- OTLP partial success is used when some records are rejected (cardinality cap, too old); the response
  says how many and why.
- Samples older than `ingest.max_age` (default 1 h) or more than `ingest.max_future` (default 10 m)
  ahead are rejected and counted.
- Spans and logs retried by the client may be stored twice. Compaction dedupes spans on
  `(trace_id, span_id)` and logs on `(timestamp_ns, service_name, body hash)`; ad-hoc queries over L0
  may show a duplicate until then (documented).

### 5.2 Series identity — **Replaces §4.4 Table 1 formula**

```
series_id = xxHash64( tenant 0x00 region 0x00 metric_name 0x00 canonical_labels )
canonical_labels = for each label sorted by key (bytewise), excluding __name__:
                   uvarint(len(key)) key uvarint(len(value)) value
```
- Length-prefixing makes the encoding injective; `0x00` inside a label value can no longer collide.
- `region` and `tenant` come from the receiving instance's config, never from the payload.
- Identity comes only from the series' own labels. `instance` is a normal Prometheus label; there is no
  node-derived input.
- **Collision check:** the series dictionary stores full labels. When an id arrives with different
  labels, increment `series_id_collisions_total` and log both label sets. At 100M series, the 64-bit
  birthday probability is ~3×10⁻⁴; the check makes a collision visible rather than silent.
- **OTel → Prometheus names:** follow Prometheus' OTLP translation strategies (underscore escaping
  with unit and `_total` suffixes by default; UTF-8 names optional), configurable, recorded per series
  so PromQL and TQS agree on a name.
- **HA Prometheus pairs:** two ids per series.
  - `series_id` hashes every label, including the replica label (default `__replica__`, also
    `prometheus_replica`), so each replica's raw data stays exact and separate.
  - `logical_series_id` hashes every label **except** the replica label. Queries group on it.
  - Read paths (PromQL `Queryable`, materializer, compactor) pick one replica per
    `(logical_series_id's cluster label, 1 m window)`: the lexicographically first replica with ≥ 1
    sample in that window.
  - Never merge the two replicas' values: scrape skew makes a merged counter dip and fake a reset.

### 5.3 Counter semantics — **New** (applies wherever a delta is computed: materializer, compactor, read-time `lag`)

| Case | `positive_delta` | `is_reset` |
| :--- | :--- | :--- |
| First sample of a series, no start time known | `0` (Prometheus `increase` semantics) | false |
| First sample, OTel cumulative with `start_time_unix_nano` > 0, or a Prometheus created timestamp, inside the lookback | `value` (counter began at start time) | false |
| `value ≥ prev` | `value − prev` | false |
| `value < prev` | `value` | true |
| OTel cumulative, `start_time` changed | `value` | true |
| OTel delta temporality | `value` (reject negative values for monotonic sums) | false |
| Same timestamp as `prev`, same value | dropped (duplicate) | — |
| Same timestamp, different value, or timestamp < `prev` | rejected as out-of-order, counted | — |
| Prometheus staleness marker (NaN, bit pattern `0x7ff0000000000002`) | stored with `is_stale = true`, `positive_delta = 0`; range functions ignore it; last point is kept | false |
| No previous point within `delta.lookback` (default 15 m) | treated as first sample | false |

Histogram buckets, `count` and `sum` follow the same table per bucket.

### 5.4 Ingest topology and membership — **Replaces §5 router stage**

- Ingest nodes are interchangeable; a load balancer may spray. No peer forwarding is needed for
  correctness (D2, D6).
- One **coordinator** role per region (leased, §5.5) runs: checkpointer, materializer, compactor,
  retention. Any node can hold the lease; losing it costs one lease timeout of commit delay, not data.
- Membership (needed for hot-tail fan-out and optional affinity): `memberlist` gossip, or a Kubernetes
  EndpointSlice watch when running on Kubernetes.
- Optional metrics affinity (post-M3): rendezvous (HRW) hashing on `logical_series_id`.

### 5.5 Commit protocol — **Replaces §3.2 / §5 commit details**

Envelope, written with `If-None-Match: *` to `commits/<tenant>/<region>/<ulid>-<node_id>.json`:

```json
{
  "format": 1,
  "envelope_id": "01JC…",
  "kind": "ingest",
  "tenant": "default",
  "region": "us-east",
  "node_id": "ing-7f3a",
  "node_epoch": 1731211200123,
  "batch_seq": { "from": 4410, "to": 4417 },
  "files": [
    {
      "table": "traces_spans",
      "path": "s3://telemetry-us-east/traces/spans/…/l0-01JC…-ing-7f3a.parquet",
      "bytes": 41234567, "rows": 812345,
      "min_ts_ns": 1731211140000000000, "max_ts_ns": 1731211199999999999,
      "schema_version": 3
    }
  ],
  "replaces": []
}
```

- `kind ∈ {ingest, compaction, retention, erasure}`. Compaction lists the L0 paths it supersedes in
  `replaces`; the checkpointer applies add + remove in **one** catalog transaction.
- **Idempotency:** the catalog has `applied_envelopes(envelope_id PRIMARY KEY, snapshot_id)`.
  Re-applying after a crash between "commit" and "delete envelope" is a no-op.
- **Envelope GC:** an envelope is deleted only after the catalog state that includes it is durable —
  in single-node mode, after the catalog snapshot that contains it has been uploaded. A lost local
  catalog is rebuilt by replaying the remaining envelopes.
- **Coordinator lease:** `catalog/<tenant>/<region>/coordinator.lease`, taken and renewed with
  conditional PUT (`If-None-Match: *` / `If-Match: <etag>`); PostgreSQL advisory lock in cluster mode.
  The lease alone is not a fence (a paused holder can wake after expiry), so **every catalog write is
  itself conditional**: on the catalog snapshot ETag (single node) or a `coordinator_epoch` column
  checked in the transaction (PostgreSQL).
- Conditional-write behaviour differs per store (S3, R2, GCS generation match, MinIO). S6 verifies
  each supported store; an unsupported store is refused at startup with a message naming it.

### 5.6 Hot-tail consistency protocol — **Replaces failure mode #6 and §6.1 "MVCC" text**

`batch_seq` is per `(node_id, node_epoch)`, so one global watermark does not exist. The protocol:

1. The query pins catalog snapshot `S` and reads `W(S)`: the committed `batch_seq.to` per
   `(node_id, node_epoch)` from `applied_envelopes`.
2. It sends the query fragment to each live node in scope with that node's `W(S)[node]`.
3. A node returns rows from batches with `seq > W(S)[node]`, plus its **retained-from** watermark `R`.
4. A node keeps a batch in RAM until the catalog shows it committed **and** a grace period
   (default 2 × checkpoint interval) has passed — not merely until the S3 PUT.
5. If any node reports `R > W(S)[node]` (it already dropped rows the snapshot does not include), the
   query re-pins a newer snapshot and retries, at most twice, then answers with a partial-result
   warning.
6. Cross-region hot tails have a 150 ms budget; a missed region yields a partial-result warning that
   names the region. Laptop / `--read-only` mode has no hot tail and says so: freshness is flush
   interval + checkpoint interval (~65 s).

### 5.7 Physical schema corrections — **Amends §4**

**All tables:** add `tenant VARCHAR` (RLE dictionary, first sort key). Every file carries
`schema_version` in Parquet key-value metadata.

**`traces_spans`:**
- Sort `(trace_id, start_time_ns)`. Trade-off accepted: service-level span scans no longer prune by
  sort order; service-level search goes through `traces_index` and span-metrics views.
- `trace_id`: Parquet `FIXED_LEN_BYTE_ARRAY(16)` with `UUID` logical type (DuckDB `UUID`, ordered
  compare). `span_id`/`parent_span_id`: `UBIGINT`.
- Split-block bloom filter on `trace_id` written by our writer (`parquet-go`) on L0 **and** L1.
- Add `trace_state VARCHAR`, `scope_name VARCHAR`, `scope_version VARCHAR`, `dropped_attrs_count`,
  `dropped_events_count`, `dropped_links_count`.
- Promoted attributes (`http_status_code`, later more) are a versioned list. Promotion adds a nullable
  column; readers use `coalesce(promoted, TRY_CAST(span_attrs[key] AS …))` so files from before the
  promotion still answer.

**`traces_index`:** add `has_root BOOLEAN`, `root_status_code UTINYINT`, `root_duration_ns UBIGINT`
(nullable, set where the root was seen). Trace duration = `max(max_end_ns) − min(min_start_ns)` over
segments.

**`metrics_series`:** append-only, **one row per `(series_id, day)`** the series was seen. Series
resolution for a time range then prunes by day. "Newly discovered" means new for that day (a per-day
Roaring64 set, reset at UTC midnight). Drop the mutable `last_seen_ms`. Add `logical_series_id`,
`unit`, `description` (first seen value).

**`metrics_points_raw`:** add `is_stale BOOLEAN`. `positive_delta` is **nullable** and filled by the
compactor (L1) only (D2). `metric_type` enum adds `5 = Summary`.

**`metrics_histograms_raw`:** replace `tdigest_sketch` and `positive_buckets LIST` with:
`schema TINYINT, zero_threshold DOUBLE, zero_count UBIGINT, positive MAP(SMALLINT, UBIGINT),
negative MAP(SMALLINT, UBIGINT), count UBIGINT, sum DOUBLE, min DOUBLE, max DOUBLE,
explicit_bounds LIST(DOUBLE), explicit_counts LIST(UBIGINT), reset_hint UTINYINT`
— raw cumulative values; per-bucket deltas follow §5.3.

**Rollups (`metrics_rollup_5m`, `_1h`):** `tdigest_sketch` → `hist MAP(SMALLINT, UBIGINT)` at the
configured scale (D8). Produced by the compactor (B11).

**`logs_records`:**
- `pattern_hash` = xxHash64 of the **masked** body: a fixed, versioned tokenizer replaces numbers,
  UUIDs, hex ≥ 8 chars, IPv4/IPv6, quoted strings, e-mail addresses and durations with `<*>`.
  Store `pattern_version UTINYINT`. Never learn templates per node.
- Body search is a brute-force scan after time/service/severity pruning — the same trade Loki makes.
  A token bloom index is a later optimisation, not v1.

### 5.8 Materialized panels — **Replaces §6.2**

**Input:** the coordinator's materializer reads new files from `kind = ingest` envelopes as it applies
them — not the DuckLake change feed. Compaction, retention and erasure envelopes never feed views.

**State:** one table for every panel, long format:

```
views_partials(tenant, panel_id, panel_version, bucket_ts, region, group_key VARCHAR,  -- canonical JSON of group values
               agg_name, n BIGINT, s DOUBLE, mn DOUBLE, mx DOUBLE,
               last_ts BIGINT, last_val DOUBLE, hist MAP(SMALLINT, UBIGINT))
```
Open buckets live in materializer RAM and are queried from there; a bucket is written once it is older
than `view.seal_after` (default 2 min). Late rows append another partial row; reads merge.

**Alignment:** `bucket_ts = floor(ts_ms / step_ms) * step_ms`, UTC epoch. Steps of 1 day or more align
to UTC midnight; no time zones in v1.

**Mergeable aggregations:**

| TQS op | Partial state | Merge | Finalize |
| :--- | :--- | :--- | :--- |
| `count`, `error_count` | `n` | `SUM` | `n` |
| `sum` | `s` | `SUM` | `s` |
| `counter_increase` | `s` (materializer delta, §5.3) | `SUM` | `s` |
| `counter_rate` | `s` | `SUM` | `s / step_seconds` (per second) |
| `gauge_avg` | `s`, `n` | `SUM`, `SUM` | `s / n` — **sample-weighted**, not `avg(avg_over_time)`; documented |
| `gauge_min` / `gauge_max` | `mn` / `mx` | `MIN` / `MAX` | — |
| `gauge_last` | `last_ts`, `last_val` | `arg_max(last_val, last_ts)` | — |
| `p50…p99`, `quantile(q)` | `hist` | `SUM` per bucket | cumulative walk; bucket geometric midpoint |
| `count_distinct` | not materializable in v1 (needs HLL) | — | ad-hoc only |

`formulas[]` are evaluated after the merge.

**Lifecycle:** changing a panel's TQS bumps `panel_version` and backfills the visible range from the
lake. Views have their own TTL (default = the shortest retention of the data they read). Erasure
(§5.15) invalidates affected buckets and rebuilds them.

**Demand tiers:** keep v5's lease tiering (active panels and all alert/recording rules tick; cold ones
catch up on open).

**SLO:** materialized read p95 < 50 ms server-side; checkpoint-to-visible < 15 s.

### 5.9 TQS v2.1 — **Amends §7**

Changes to the schema:
1. Add `"tqs_version": {"const": "2.1"}` (required) and `additionalProperties: false` at every level.
2. Per-signal validity with `if/then`: `target` values per signal; `trace_id` required when
   `target = trace_waterfall`; `counter_*` and `gauge_*` only for `signal = metrics`;
   `field: duration_ns` only for traces.
3. **Filter keys** match `^(?:[a-z_]+|(?:labels|resource_attrs|span_attrs|log_attrs)\.[A-Za-z_][A-Za-z0-9_.:/-]{0,127})$`.
   A bare key must be a column of the signal's table (list published by `discover_telemetry_catalog`).
   Values are always bound parameters.
4. **Types on map values:** map values are strings. `gt/gte/lt/lte` on them compile to
   `TRY_CAST(… AS DOUBLE)`; a non-numeric value fails the predicate. `eq/neq/in` compare as strings
   unless the value is a number, in which case the same cast applies.
5. **`regex`:** RE2 syntax, fully anchored (PromQL semantics), max 256 chars.
6. **Time:** `start`/`end` accept a relative duration (`"15m"` = 15 minutes before `end`), RFC 3339, or
   epoch ms. `end` defaults to `now`. Max points per series = 11,000 (the Prometheus limit); `step`
   defaults to `range / 300` rounded up to a whole second.
7. **Limits:** `limit` applies to output groups (top-N by `order_by`); `max_groups` (default 1,000,
   max 10,000) caps `group_by` cardinality — exceeding it returns an error that names the dimension.
8. **New ops:** `quantile` with `q` (0–1) for metrics histograms and span durations; `count_distinct`
   (ad-hoc only).
9. **Logs:** `body_contains` (case-sensitive substring) and `body_regex` (RE2, unanchored) as filter
   keys for `signal = logs`; `target = patterns` groups by `pattern_hash`.
10. `formulas[].expr` uses this grammar and nothing else:

```ebnf
expr    = term , { ("+" | "-") , term } ;
term    = factor , { ("*" | "/") , factor } ;
factor  = number | alias | "(" , expr , ")" | "-" , factor
        | func , "(" , expr , { "," , expr } , ")" ;
func    = "abs" | "least" | "greatest" | "coalesce" | "round" | "clamp" ;
alias   = an "as" name of aggregations[] or of an earlier formulas[] entry ;
number  = decimal literal ;
```
`/` returns NULL when the divisor is 0, so `NULLIF` is not needed. The compiler parses to an AST and
emits SQL from the AST; the string never reaches SQL.

Corrected Example 1 (the request-code label in Prometheus conventions is usually `code`):

```json
{
  "tqs_version": "2.1",
  "id": "global_checkout_error_pct",
  "signal": "metrics",
  "target": "http_requests_total",
  "time_range": { "start": "1h" },
  "step": "1m",
  "filter": { "service_name": "checkout-api" },
  "group_by": ["region"],
  "aggregations": [
    { "op": "counter_increase", "as": "errors_5xx", "filter": { "labels.code": { "regex": "5.." } } },
    { "op": "counter_increase", "as": "total_requests" }
  ],
  "formulas": [ { "as": "error_rate_pct", "expr": "errors_5xx / total_requests * 100" } ]
}
```

In Example 2, drop the redundant `filter` on `error_count`: `error_count` on traces is defined as
`status_code = 2`.

### 5.10 PromQL compatibility — **Replaces §6.3**

- Endpoints: `/api/v1/query`, `/query_range`, `/series`, `/labels`, `/label/<name>/values`,
  `/metadata`, `/status/buildinfo` (Grafana calls the last two).
- **Engine:** the upstream Prometheus `promql.Engine`. We implement `storage.Queryable`:
  `Select(matchers, hints)` resolves matchers against `metrics_series` (day-pruned) and streams samples
  from DuckDB (raw, or a rollup when `hints.Step` and `hints.Range` allow), plus the hot tail.
- Exact Prometheus semantics — extrapolation, staleness, lookback delta, vector matching,
  `histogram_quantile` on classic `_bucket` series — then come for free.
- **Pushdown** (after correctness): `sum by (…) (rate(x[r]))` and `histogram_quantile(…)` compiled to
  DuckDB when the step is aligned to a rollup bucket. Rollups are used only when `step` and `range`
  are multiples of the rollup size and the window is aligned; otherwise raw.
- **Native and OTel histograms** are exposed to PromQL as native histograms.
- **Gate:** the Prometheus PromQL compliance tester against a reference Prometheus on the same data.
  M1 target ≥ 95 %; failures listed by function.
- Limits: max series per query, max samples per query (default 50M, as Prometheus), query timeout.

### 5.11 Multi-region — **Amends §3.1**

- One bucket per region (`telemetry-<region>`), or one R2 bucket per jurisdiction. Ingest writes only
  to its own region's bucket.
- One logical catalog. The catalog stores absolute paths across buckets (S1 verifies DuckLake can);
  DuckDB uses one scoped secret per bucket.
- Catalog placement: PostgreSQL primary in one region, read replicas in the others; queries read
  catalog metadata from the local replica. Commits are coordinator → primary (cross-region, async to
  ingest, so ingest latency is unaffected).
- **Residency:** a `residency` policy per region (`global` | `in-region-only`). An in-region-only
  region is never read by a node or laptop outside it; the global read lane returns its aggregates
  only from that region's nodes (federated), never raw rows.
- Cross-region egress is billed on S3 reads from other regions. The planner prefers federating an
  aggregate to the owning region over pulling raw Parquet across regions.

### 5.12 Grafana read compatibility — **New** (v5 accepts Loki push but offers no Loki/Tempo *query* API)

| Grafana data source | Endpoints | Milestone |
| :--- | :--- | :--- |
| Prometheus | §5.10 | M1 |
| Tempo | `/api/traces/<id>`, `/api/v2/traces/<id>`, `/api/search` (tags + duration + status), `/api/search/tags`, `/api/v2/search/tag/<t>/values` | M2 |
| Loki | `/loki/api/v1/query_range`, `/query`, `/labels`, `/label/<n>/values`, `/series` — a LogQL **subset**: stream selector, line filters, `| json`, `| logfmt`, label filters, `count_over_time`, `rate`, `sum by` | M2 |

Implemented clean-room against the public HTTP APIs; no AGPL code (D9). Unsupported LogQL returns
400 with the unsupported construct named.

### 5.13 Alerting and recording rules — **New** (v5 mentions "Alert Rules" but never defines them)

- Accept Prometheus rule files (`groups → rules`, recording and alerting) unchanged; evaluate with the
  upstream `rules.Manager` over our `Queryable`.
- Recording rules write back as series (`kind = ingest` envelopes from the coordinator).
- Alerts are sent to Alertmanager over its v2 API; we do not reimplement notification routing.
- TQS-based alerts (`condition` over a materialized panel) come after M4.
- Rule evaluation runs on the coordinator; evaluation lag and failures are self-metrics.

### 5.14 Compaction, rollups, retention — **Replaces §5.1 item 3 and failure modes #12, #16**

- **Flush trigger:** size (default 64 MB uncompressed) **or** age (default 60 s), whichever comes
  first. Low-volume tenants therefore produce small L0 files; compaction fixes that.
- **Compactor** (coordinator): per `(tenant, region, table, hour)`, after `hour end + grace`
  (default 10 min). Reads L0 with DuckDB, sorts, dedupes (§5.1), recomputes `positive_delta` (§5.3),
  writes L1 with our Parquet writer (bloom filters, page index, row groups of 122,880 rows), then
  commits add + remove in one `compaction` envelope. Rollups 5 m/1 h are produced in the same pass.
- **Late data** after compaction lands in a new L0 file for that hour; a second compaction pass picks
  it up (generation counter in the file name).
- **Retention:** per `(tenant, signal)`. The coordinator removes files from the catalog, waits for
  `snapshot_retention` (so pinned snapshots still read), then deletes objects. S3 lifecycle rules are
  a backstop only, set to retention + 7 days.

### 5.15 Security, tenancy and privacy — **New**

- **AuthN:** bearer tokens (static hashed list in config for v1; OIDC later) on ingest, query and MCP
  HTTP. mTLS for node-to-node RPC. The public `/v1/rum` endpoint (post-v1) uses per-app public keys,
  origin allow-lists and per-IP rate limits.
- **AuthZ:** token → `{tenant, scopes: ingest | query | admin}`. MCP inherits the token's scopes; MCP
  write tools need `admin`.
- **Laptop mode:** needs read credentials to the bucket. Document a dedicated read-only IAM role
  scoped to a tenant prefix and to non-`in-region-only` regions; never reuse the ingest credentials.
- **Data in telemetry:** ingest-time redaction rules (regex + attribute deny-list) run before Parquet.
  Defaults mask `authorization`, `cookie`, `set-cookie`, `password`, `token`, `api_key` attributes and
  bearer tokens in log bodies.
- **Erasure:** `erasure` envelopes rewrite affected files (filter + rewrite + replace in one commit),
  then invalidate view buckets. Erasure is a batch job measured in hours, and the docs say so.
- **Query cost limits** (all surfaces, MCP included): DuckDB `memory_limit` and `threads` per query,
  timeout (default 30 s, MCP 15 s), max bytes scanned (default 10 GB), max concurrent queries per
  token.
- **License:** Apache-2.0; dependency licenses checked in CI (D9).
- **Threat model** to be written before M3: tenant isolation in the query compiler, MCP prompt
  injection through telemetry content (log bodies are attacker-controllable text shown to an agent),
  credential exposure in laptop mode.

### 5.16 MCP changes — **Amends §8**

- Inline the TQS schema in tool definitions (no remote `$ref`).
- Every result carries `rows_returned`, `rows_total`, `truncated`, `bytes_scanned`, `snapshot_id`,
  `freshness_s`, `partial_regions`. Default output budget ≈ 8k tokens; truncation says how to narrow.
- Telemetry text (log bodies, span names, attribute values) is returned inside a clearly delimited
  data field. The server never puts it into the tool's own descriptive text, which limits prompt
  injection from logs.
- `save_materialized_dashboard_panel` is absent in `--read-only` mode and requires `admin` scope.
- Add `search_logs` (thin TQS wrapper; agents use it far more than raw TQS) and `list_panels`.
- A raw read-only SQL tool is **not** in v1: it bypasses tenancy and cost controls.
- **`correlate_anomalies`** algorithm (v5 names none): for each candidate dimension (region,
  `service_name`, top-50 attribute keys by presence, `pattern_hash`), compare the share of each value
  in the incident window against the baseline window. Score by lift × support with a χ² test
  (p < 0.01). Return the top 10 dimension values with incident share, baseline share, lift, count, and
  one example `trace_id` or log line each. Candidate keys and values are capped (50 × 100) to bound
  cost.

### 5.17 Query execution and caching — **New**

- Footer/metadata cache (RAM, LRU) keyed by object path + ETag: a cold query otherwise pays one or two
  GETs per file before reading data.
- Range-block cache (RAM + optional local disk, LRU), using DuckDB's external file cache where
  available, else our own object-store layer.
- Planner pruning before DuckDB: tenant → region → time → table → catalog column stats; DuckDB then
  applies row-group stats and bloom filters.
- The SLOs in v5 §6.1 (40–300 ms ad hoc) become **targets measured in S3** (§8), per data volume.

### 5.18 Operability — **New**

- One YAML config file; every key also settable by env var; startup prints the effective config with
  secrets redacted.
- Self-telemetry: Prometheus `/metrics` (ingest rates, rejected samples by reason, flush latency,
  arena use, commit lag, compaction backlog, query latency, bytes scanned, cache hit rates) and OTLP
  traces of its own queries.
- **Format versioning:** `format` in envelopes and `schema_version` in files. A binary refuses to
  write to a catalog with a newer major format and names both versions. Upgrades are forward-only;
  each release states the oldest format it reads.
- Health: `/ready` fails while the arena is over 95 % or the catalog is unreachable; `/live` only on
  deadlock.

### 5.19 Cost model — **New** (replaces the single per-GB figure)

Per region per day, for planning (list prices; verify for your account):

| Item | S3 Standard | R2 |
| :--- | :--- | :--- |
| Storage | $0.023/GB-month | $0.015/GB-month |
| L0 PUTs (10 nodes × 4 tables × 1/min) ≈ 58k/day | ~$0.29 | ~$0.26 |
| Commit envelope LIST every 5 s ≈ 17k/day | ~$0.09 | ~$0.08 |
| Query range GETs | $0.0004 / 1k | $0.36 / M |
| Cross-region read egress | $0.02+/GB | $0 |

The dominant cost is storage × retention × bytes/sample (S2 measures the last factor). `s3-wal` mode
adds ~86k PUT/day/node.

---

## 6. Replaced stress-test rows (§10)

| # | v5 row | Replacement |
| :--- | :--- | :--- |
| 1 | Consistent hash peer routing | Stateless ingest; routing optional (D2, D6). |
| 5 | Boundary-linked `series_last_point` on owner nodes | Single-writer delta computation in materializer / compactor (D2, §5.3). |
| 6 | Global `batch_seq_id` watermark | Per-node watermark protocol with re-pin (§5.6). |
| 9 | (kept) Partial-segment `traces_index` | Unchanged, plus `has_root`, root fields (§5.7). |
| 12 | `region + instance_id` in hash | Length-prefixed canonical encoding; no node-derived input (§5.2). |
| 16 | L0 → L1 compactor | Compactor also recomputes deltas, writes rollups and blooms (§5.14). |
| 17 | `If-Match` on snapshot | Plus fencing on every catalog write (§5.5). |
| 19 | 60 s `(series_id, ts)` dedupe | Replica label kept; one replica selected per `(cluster, 1 m)` on read (§5.2). |
| — | **New #21** Acknowledged data lost on crash | Durability modes (§5.1). |
| — | **New #22** Compaction double-counted in views | Views fed from `ingest` envelopes only (§5.8). |
| — | **New #23** Lifecycle deletes referenced files | Catalog-first retention (§5.14). |
| — | **New #24** Agent query exhausts node | Per-query cost limits (§5.15). |
| — | **New #25** Prompt injection via log content | Delimited data fields in MCP results (§5.16). |

---

## 7. Replaced verification suite (§10.1)

1. **Counter accuracy under crash.** 3 regions × 1,000 req/s, `kill -9` one ingest node per region
   mid-run, durability `memory`. Pass: global `counter_increase` over the run equals the generator's
   ground truth exactly for cumulative counters; gauge/span loss ≤ one flush interval of that node's
   share. Repeat with `wal` and `replicated`: zero loss.
2. **PromQL compliance.** Prometheus compliance tester vs. reference Prometheus: ≥ 95 % in M1.
3. **Trace stitching.** 10k traces across 3 regions and flush boundaries. Pass: 100 % span
   reconstruction; p95 lookup latency measured and published (target < 300 ms cold, < 80 ms warm).
4. **Object-store outage.** 5-minute blackout (Toxiproxy) at 100k spans/s. Pass: RSS stays under
   the configured ceiling, `429` starts at the documented threshold, no acknowledged data is lost in
   `wal`/`replicated` modes, full catch-up after recovery.
5. **1,000 materialized panels.** Pass: tick CPU < 250 ms, read p95 < 50 ms, no change after a forced
   compaction of the same hours (catches B5).
6. **Hot-tail transition.** 50 ms polling across flush + commit + node drop. Pass: monotonic results,
   no double count, no dip.
7. **HA pair.** Two Prometheus replicas with 2 s scrape skew. Pass: no false resets; `sum(rate())`
   within 1 % of a single Prometheus.
8. **Compression.** S2 harness on node_exporter + kube-state-metrics data. Publish bytes/sample.
9. **MCP evaluation.** A fixed set of incident scenarios on seeded data; measure the share an agent
   resolves with only the five tools, and tokens used.

---

## 8. Spike plan — before any production code (≈ 2 weeks)

Each spike has a pass/fail exit. A failed spike changes a decision, named in the last column.

| # | Question | Pass when | On fail |
| :--- | :--- | :--- | :--- |
| S1 | Can DuckLake be the catalog for externally written files? | (a) `ducklake_add_data_files` registers `parquet-go` files with `UUID`, `MAP`, `LIST`, `STRUCT` columns and populates min/max stats used in pruning; (b) one transaction can remove L0 files and add an L1 file; (c) a PostgreSQL catalog takes 1 commit/5 s × 50 files with p99 < 200 ms; (d) absolute paths in two buckets in one catalog; (e) read-only attach of a DuckDB-file catalog from S3; (f) **observe** whether the change feed re-emits compacted rows (recorded, not gating — §5.8 does not use it). | D3 → Option B (own manifest). |
| S2 | Bytes/sample for `metrics_points_raw` | Measured on real exporter data with `BYTE_STREAM_SPLIT`+ZSTD vs `DELTA`/ZSTD variants; ≤ 4 B/sample is a reasonable bar | Revisit: drop `positive_delta` from L0 (already nullable), XOR-encode values in a `BLOB` per series-chunk. |
| S3 | Query latency on object storage | 1 day × 1 region of realistic volume in S3 and R2: trace lookup, 1 h metric range, 24 h log grep — p50/p95 cold and warm | Adjust SLOs; prioritise §5.17 caches. |
| S4 | Prometheus engine over DuckDB | `storage.Queryable` prototype passes the compliance tester's basic suite; 1 h `sum(rate())` over 10k series < 1 s | D7 stands; scope pushdown earlier. |
| S5 | Hot tail via `duckdb-go` Arrow | Register an Arrow record batch as a view and `UNION ALL` it with Parquet; measure overhead for 1M rows | Serve hot tail through our own scan, not DuckDB. |
| S6 | Conditional writes per store | `If-None-Match: *` and `If-Match` on S3, R2, GCS (generation match), MinIO: a racing writer gets 412 | Unsupported stores are PostgreSQL-catalog only. |

S1 needs network access to the DuckDB extension repository and the DuckLake docs, which this
review's environment did not have.

---

## 9. Revised roadmap

Estimates assume 2–3 engineers and are ranges, not commitments. They replace v5 §11 ("6 weeks").

| Milestone | Scope | Estimate |
| :--- | :--- | :--- |
| **M0 Spikes** | S1–S6; decisions D1–D10 signed off | 2 weeks |
| **M1 Single-node metrics** | Prometheus `remote_write` + OTLP metrics; `wal` durability; Parquet writer; catalog (D3); envelopes; compactor + rollups + retention; PromQL via upstream engine; Grafana Prometheus data source works; self-metrics | 6–8 weeks |
| **M2 Traces + logs + agents** | OTLP traces/logs, Loki push; `traces_index`; Tempo + Loki read subsets; TQS v2.1 ad hoc; MCP read-only tools + laptop mode; auth tokens | 6–8 weeks |
| **M3 Cluster** | N stateless nodes; coordinator lease + fencing; hot-tail fan-out (§5.6); `replicated` durability; threat model | 4–6 weeks |
| **M4 Materialized + rules** | §5.8 panels; Prometheus rule files; Alertmanager; `correlate_anomalies` | 4–6 weeks |
| **M5 Multi-region** | Per-region buckets; catalog replicas; residency; federated aggregates | ~4 weeks |
| **Post-v1** | Prometheus scrape mode, TSDB/Loki importers, Grafana dashboard migrator, embedded UI, RUM + sourcemaps | — |

**v1.0 ≈ M0–M5: roughly 6–9 months.** The first usable release — "point Prometheus `remote_write` and
Grafana at it, keep a year of metrics on S3" — is M1, at about 2–2.5 months.

---

## 10. Open questions for the team

1. **RPO** users accept by default (drives D4). Is "lose ≤ 60 s of gauges/spans/logs on node loss"
   acceptable for the single-node product?
2. **Tenancy** in v1: single tenant with the column in place (D5), or real multi-tenant from M3?
3. **Product name** — needed for the schema `$id`, binary name and bucket layout; replace `myengine` /
   `fanout-lake.dev`.
4. **License** final choice (D9) and whether a CLA is needed.
5. **Primary target store**: S3 or R2 first? It changes the cost story, conditional-write details and
   which S3 benchmark to run first.
6. **Who owns S1** and has network access to run it.
