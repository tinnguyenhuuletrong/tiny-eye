# Master Architecture & Product Specification: Blob-First, AI-Native Observability Engine

**Version:** 5.0 (Complete Master Specification — Consolidating Storage, S3 Bucket Formats, TQS v2, PromQL, MCP, Drop-In Migration & Multi-Region Hardening)  
**Target License:** Open Source (Apache 2.0 / MIT)  
**Core Stack:** Rust or Go + Apache Arrow/Parquet + DuckDB (`libduckdb`) + DuckLake + S3/R2 Object Storage  

---

## 1. Executive Summary & Core Vision

Modern engineering teams are exhausted by the **"Observability Tax"**: operating Prometheus, Grafana, Loki, Tempo, and Mimir (the LGTM stack) requires managing 15+ interconnected microservices, three distinct query languages (PromQL, LogQL, TraceQL), fragile local persistent volumes, and heavy baseline RAM consumption. Conversely, single-binary tools like `fanout.run` demonstrate the developer experience benefits of **DuckDB + Parquet + AI/MCP**, but remain restricted to a single node's local disk with no object storage tier, no horizontal scalability, and no multi-region story.

### 1.1 Our Thesis
We are building an **open-source, single-binary, stateless observability engine** that treats **Blob Storage (AWS S3, Cloudflare R2, MinIO, GCS)** as the Day-1 primary storage tier, powered by **DuckLake + ZSTD Parquet**, an embedded **DuckDB** vectorized query engine, and an **AI-first Declarative Telemetry Query Spec (TQS v2)**.

### 1.2 Core Architectural Targets
1. **100% Stateless Compute Binary:** Scales from a single container to $N$ instances across multiple global regions without Kubernetes `StatefulSets`, local PVC disks, catalog lock contention, or counter reset corruption.
2. **Blob Storage First (Day-1 S3/R2 Native):** Infinite telemetry retention at $\sim \$0.015\text{/GB/month}$ (and $\$0$ egress fees on Cloudflare R2) using sorted, ZSTD-compressed Apache Parquet files with native Split-Block Bloom Filters and Page Indexes.
3. **Unified Global Read Lane Across All Regions:** Whether the same active-active service runs across 5 regions or different services run in isolated regions, the Read Lane provides a unified global view with mathematically exact metrics and stitched cross-region distributed traces.
4. **Dual-Mode DuckDB Query Engine:**
   - **Ad-Hoc Full Scan Mode:** Vectorized HTTP Range reads over S3 Parquet using DuckLake Zone Maps and Parquet Bloom Filters, seamlessly unioned with MVCC-watermarked in-memory Arrow buffers for **$0\text{ms}$ real-time freshness**.
   - **Incremental Materialized Dashboard Mode:** Sub-millisecond ($< 1\text{ms}$) dashboard reads powered by DuckLake Change Data Capture (`ducklake_table_insertions`), Data Inlining, Shared-Scan delta evaluation, and associative Merge-On-Read formulas.
5. **Zero-Friction Drop-In Replacement:** Native ingestion for OpenTelemetry (OTLP gRPC/HTTP), Prometheus (`remote_write` + `--prometheus-config` pull scraping + PromQL `ASOF JOIN` compatibility), and Grafana Loki (`/loki/api/v1/push`), plus an AI-assisted Grafana Dashboard Importer with shadow validation.
6. **AI-First by Design:** Built-in Model Context Protocol (MCP) server allowing AI agents (Claude Code, Cursor, custom runbooks) and developer laptops to query production S3 buckets directly with **zero production compute impact**.

---

## 2. Market Landscape & Lessons Learned

### 2.1 Comparative Matrix

| Solution | Signals Covered | Primary Storage Backend | Object Storage Support | Query Engine / Language | Operational Complexity | Strengths | Major Weaknesses & "Nightmares" |
| :--- | :--- | :--- | :--- | :--- | :--- | :--- | :--- |
| **Prometheus** | Metrics only | Local custom TSDB (WAL + 2h blocks) | None natively | PromQL | **Low** (Single binary) | De facto standard; ubiquitous scraping & K8s ecosystem. | No long-term storage; single-node vertical limit; high-cardinality labels cause OOM-kill loops. |
| **Thanos** | Metrics only | S3 / GCS / Azure via sidecar or receive | Bolted-on | PromQL (Thanos Querier) | **Medium–High** | Extends existing Prometheus; cheap S3 cold storage; downsampling. | Multiple microservices (`sidecar`, `store`, `compact`, `query`, `ruler`); high latency scanning uncompacted S3 chunks. |
| **VictoriaMetrics** | Metrics (Logs via VictoriaLogs) | Custom MergeTree on Local SSD or S3 (Enterprise) | Tiered / Enterprise | MetricsQL (PromQL drop-in) | **Low** (Single) to **Medium** (Cluster) | $5\times$–$10\times$ less RAM/CPU than Prometheus; $< 1\text{ byte/sample}$ compression. | Custom non-standard storage format; lacks a unified columnar engine for distributed trace graphs + AI SQL. |
| **Grafana LGTM** *(Loki, Tempo, Mimir)* | Metrics, Logs, Traces | S3 / Object Storage (Day-1 design) | Native Day-1 | PromQL, LogQL, TraceQL | **Extreme** ($15+$ microservices) | Native Grafana UI correlation; massive multi-tenancy; object storage backed. | 15+ microservices (distributor, ingester, querier, compactor, store-gateway); 3 separate query languages; heavy idle resource tax. |
| **SigNoz** | Metrics, Logs, Traces (OTel-native) | ClickHouse (Local SSD or S3 MergeTree) | Tiered S3 Disks | SQL + ClickHouse dialect | **Medium–High** | All 3 signals in one DB; fast aggregations; single UI; native OTel. | ClickHouse is memory-hungry (needs $16\text{GB}+$ RAM to breathe); complex schema migrations; ZooKeeper/Keeper overhead. |
| **Fanout.run** | Metrics, Logs, Traces | Local Parquet | **None** | DuckDB SQL | **Very Low** (Single binary) | Lightweight single binary; DuckDB + Parquet + native MCP server. | Strictly trapped on single local disk; cannot scale compute/storage independently or span regions. |
| **Our Engine** | **Metrics, Logs, Traces, RUM** | **S3/R2 Parquet + DuckLake** | **Native Day-1** | **TQS v2 + DuckDB SQL + PromQL** | **Very Low** (Stateless Single Binary) | **Zero-disk stateless scaling; pennies/TB on S3/R2; $< 1\text{ms}$ incremental views; local laptop MCP queries.** | *Solved via our hardened v5 architecture below.* |

### 2.2 Key Architectural Lessons We Adopt
1. **From Grafana Tempo (`vParquet4`):** Do not build expensive inverted indexes for traces. Instead, sort spans by `(service_name, trace_id, start_time_ns)` and embed **Parquet Split-Block Bloom Filters** on `trace_id` so DuckDB can skip $99\%$ of S3 files via a single $16\text{KB}$ HTTP Range header read.
2. **From SigNoz / ClickHouse:** Separate the compact **Trace Summary Index (`traces_index`)** from raw **Spans (`traces_spans`)**. Searching for slow or failed root traces scans $10\times$ less data because child DB/HTTP spans are skipped until waterfall reconstruction.
3. **From VictoriaMetrics & Prometheus:** Never repeat full label maps (`MAP(VARCHAR, VARCHAR)`) on every 10-second metric sample. Hash the canonical label set into a deterministic 64-bit integer **`series_id`**, store the label dictionary in `metrics_series`, and store narrow `(series_id, timestamp_ms, value, positive_delta)` rows sorted by `series_id` to achieve $< 1.1\text{ bytes/sample}$ compression.
4. **From DuckLake:** Traditional S3 table formats (Apache Iceberg / Delta Lake) write thousands of slow JSON/Avro manifest files to S3, which collapses under high-frequency streaming observability writes. **DuckLake** stores file pointers, column min/max Zone Maps, and **Inlined Hot Data** inside a fast SQL catalog (SQLite/DuckDB or PostgreSQL) while keeping S3 purely for immutable Parquet files.

---

## 3. System Architecture, Multi-Region Topology & Language Choice

### 3.1 Multi-Region Stateless Topology
The engine compiles into a **single stateless binary**. In a multi-region environment (e.g., `us-east`, `eu-west`, `ap-southeast` running either active-active replicas of the *same* service or *different* regional services), each region ingests, buffers, and writes to S3 locally with **zero cross-region WAN write latency**, while the **Unified Global Read Lane** queries across all regions seamlessly.

```text
   ┌─────────────────────────────────┐         ┌─────────────────────────────────┐         ┌─────────────────────────────────┐
   │        REGION: US-EAST          │         │        REGION: EU-WEST          │         │      REGION: AP-SOUTHEAST       │
   │ (Service A + Checkout Replica)  │         │ (Service B + Checkout Replica)  │         │   (Payment + Checkout Replica)  │
   └────────────────┬────────────────┘         └────────────────┬────────────────┘         └────────────────┬────────────────┘
                    │ OTLP / Prom / Loki                        │ OTLP / Prom / Loki                        │ OTLP / Prom / Loki
                    ▼ (Local Only)                              ▼ (Local Only)                              ▼ (Local Only)
   ┌─────────────────────────────────┐         ┌─────────────────────────────────┐         ┌─────────────────────────────────┐
   │   STATELESS INSTANCE (US-EAST)  │         │   STATELESS INSTANCE (EU-WEST)  │         │ STATELESS INSTANCE (AP-SOUTH)   │
   │ • Scopes `series_id` by region  │         │ • Scopes `series_id` by region  │         │ • Scopes `series_id` by region  │
   │ • Intra-region hash ring        │         │ • Intra-region hash ring        │         │ • Intra-region hash ring        │
   │ • Bounded Arrow Ring-Buffer     │         │ • Bounded Arrow Ring-Buffer     │         │ • Bounded Arrow Ring-Buffer     │
   │ • `series_last_point` deltas    │         │ • `series_last_point` deltas    │         │ • `series_last_point` deltas    │
   │ • Writes ZSTD Parquet + Bloom   │         │ • Writes ZSTD Parquet + Bloom   │         │ • Writes ZSTD Parquet + Bloom   │
   └────────────────┬────────────────┘         └────────────────┬────────────────┘         └────────────────┬────────────────┘
                    │                                           │                                           │
                    │ PutObject + Commit Envelope               │ PutObject + Commit Envelope               │ PutObject + Commit Envelope
                    └───────────────────────────────────────────┼───────────────────────────────────────────┘
                                                                ▼
   ┌─────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
   │                            GLOBAL / MULTI-REGION BLOB STORAGE + UNIFIED DUCKLAKE CATALOG                                │
   │                                                                                                                         │
   │ • S3 / R2 Parquet Files Partitioned by Region: `s3://bucket/traces/spans/region=us-east/...`, `region=eu-west/...`      │
   │ • Lock-Free Commit Queue: `commits/<region>-<ulid>.json` batched into Unified DuckLake Catalog (Zone Maps include Region)│
   │ • Partial Materialized Views: `lake.views.<panel_id>` stores additive rows keyed by `(bucket_ts, region, service_name)` │
   └────────────────────────────────────────────────────────────▲────────────────────────────────────────────────────────────┘
                                                                │
                                       Parallel HTTP Range Reads + Federated Hot-Tail RPC
                                                                │
   ┌────────────────────────────────────────────────────────────┴────────────────────────────────────────────────────────────┐
   │                                   UNIFIED GLOBAL READ LANE (Any Instance or Laptop MCP)                                 │
   │ • Global Metrics: Sums `positive_delta` across all regions (`GROUP BY bucket_ts, service_name`) with 0 counter conflict │
   │ • Regional Drilldown: Prunes 100% of other regions' Parquet files via DuckLake `region` Zone Maps when filtered         │
   │ • Cross-Region Traces: Stitches spans from US-East -> EU-West -> AP-Southeast via Parquet `trace_id` Bloom Filters      │
   └─────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

### 3.2 Two Deployment Modes for the DuckLake Catalog
1. **Zero-Dependency Mode (Single Instance + S3/R2 Only):**
   - Requires **only an S3/R2 bucket** (zero external databases).
   - The instance maintains the DuckLake SQLite/DuckDB catalog locally and syncs snapshots to `s3://<bucket>/catalog/` using **S3 Conditional PUTs (`If-Match: <ETag>`)** to guarantee split-brain immunity.
2. **Cluster / Multi-Region HA Mode ($N$ Stateless Instances + PostgreSQL + S3/R2):**
   - Attaches DuckLake to a managed PostgreSQL instance (`ATTACH 'ducklake:postgres:...' DATA_PATH 's3://bucket/'`) or uses our **Lock-Free S3 Commit Queue (`commits/*.json`)** swept by a single elected Checkpointer coroutine.

### 3.3 Language & Tech Stack Trade-Off: Rust vs. Go

| Criteria | **Rust** | **Go** |
| :--- | :--- | :--- |
| **Memory Ceiling & GC Under OTLP Floods** | **Winner.** Zero GC pauses; deterministic memory bounds when holding $128\text{MB}$ Arrow buffers and sorting millions of spans in RAM. | GC pressure requires careful `sync.Pool` management when allocating millions of short-lived OTLP structs per second. |
| **Apache Arrow & Parquet Ecosystem** | **Winner (`arrow-rs` + `parquet` crate).** The official Apache Rust `parquet` crate gives finest low-level control over Row Group sorting, Page Indexes, `BYTE_STREAM_SPLIT`, and Split-Block Bloom Filters. | `parquet-go` (used by Grafana Tempo / PolarSignals FrostDB) and `apache/arrow/go` are mature and production-proven, with slightly higher RAM overhead. |
| **DuckDB Integration (`libduckdb`)** | `duckdb-rs` wraps C++ `libduckdb` via FFI and supports zero-copy Arrow `RecordBatch` registration (`duckdb_arrow_scan`). | `go-duckdb` uses `CGO` to link `libduckdb` and supports Arrow zero-copy. |
| **OTel & Prometheus Ecosystem Velocity** | `tonic` + `opentelemetry-proto` + `promql-parser` (from GreptimeDB/TiKV) provide fast native building blocks. | **Winner.** OTel Collector and Prometheus itself are written in Go; upstream scrape/discovery packages can be imported directly. |

> **Architectural Pro-Tip (The Zero-CGO Ingest Separation):** Because our Ingest Lane decodes OTLP/Prometheus into Apache Arrow, sorts `RecordBatch`es in memory, writes Parquet via native Rust (`parquet` crate) or Go (`parquet-go`), uploads via S3 `PutObject`, and writes a 1KB JSON Commit Envelope, the **Ingest Path does not even depend on C++ `libduckdb`!** `libduckdb` is only invoked by the Checkpointer, Materializer, and Query/MCP Lane.

---

## 4. Storage Layer, S3 Bucket Physical Format & Signal Schemas

### 4.1 Physical S3 Bucket Directory Hierarchy & File Naming Format
Even though DuckLake tracks files via SQL catalog pointers rather than slow S3 `LIST` calls, enforcing a clean, deterministic S3 key hierarchy enables zero-cost S3 Lifecycle TTL rules, regional data residency inspection, and disaster recovery:

```text
s3://<bucket-name>/
  ├── catalog/
  │   ├── snapshot-latest.ducklake                           # Frozen read-only catalog for local CLI / Laptop MCP agents
  │   └── checkpointer.lock                                  # Conditional-PUT lease file (Zero-DB mode)
  ├── commits/
  │   └── <region>-<ulid>-<node_id>.json                     # Lock-free 1KB commit envelopes awaiting 5s checkpoint
  ├── traces/
  │   ├── spans/
  │   │   └── region=us-east/year=2026/month=11/day=10/hour=04/
  │   │       ├── l0-<ulid>-<node_id>-zstd.parquet           # 60s flush file (16MB–128MB, Bloom-filtered)
  │   │       └── l1-<ulid>-compacted-zstd.parquet           # Hourly compacted file (128MB–256MB)
  │   └── index/
  │       └── region=us-east/year=2026/month=11/day=10/hour=04/
  │           └── idx-<ulid>-zstd.parquet                    # 10x smaller trace summary & link graph index
  ├── metrics/
  │   ├── series/
  │   │   └── region=us-east/year=2026/month=11/day=10/
  │   │       └── ser-<ulid>-zstd.parquet                    # Newly discovered series_id dictionary rows
  │   ├── raw/
  │   │   └── region=us-east/year=2026/month=11/day=10/hour=04/
  │   │       └── pts-<ulid>-zstd.parquet                    # High-res scalar points (< 1.1 bytes/sample)
  │   ├── histograms/
  │   │   └── region=us-east/year=2026/month=11/day=10/hour=04/
  │   │       └── hist-<ulid>-zstd.parquet                   # Native / Exponential histogram bucket arrays
  │   ├── rollup_5m/
  │   │   └── region=us-east/year=2026/month=11/day=10/
  │   │       └── r5m-<ulid>-zstd.parquet                    # Pre-aggregated 5-minute rollups
  │   └── rollup_1h/
  │       └── region=us-east/year=2026/month=11/
  │           └── r1h-<ulid>-zstd.parquet                    # Pre-aggregated 1-hour rollups
  ├── logs/
  │   └── records/
  │       └── region=us-east/year=2026/month=11/day=10/hour=04/
  │           └── log-<ulid>-zstd.parquet                    # Structured logs + Drain3 pattern_hash
  ├── views/
  │   └── panel_id=<panel_slug>/year=2026/month=11/
  │       └── mv-<ulid>-zstd.parquet                         # Flushed historical materialized dashboard buckets
  └── rum/
      └── sourcemaps/
          └── service=<app>/release=<git_sha>/
              └── <bundle_hash>.map.zstd                     # Compressed JS sourcemaps for stack unminification
```

### 4.2 Low-Level Parquet Writer Configuration
Every Parquet file written to S3 adheres to strict analytical tuning for DuckDB `httpfs` range reads:
- **Compression Codec:** `ZSTD` at Level 3 (optimal balance of CPU throughput and compression ratio).
- **Row Group Size:** `65,536` to `122,880` rows (exact multiples of DuckDB's `2,048` vector size for vectorized SIMD scans).
- **Page Index & Offset Index:** `write_page_index = true` and `write_statistics = true` enabled on all columns so DuckDB can issue narrow HTTP Range requests for specific data pages without downloading whole row groups.
- **Column Encodings:**
  - Timestamps (`INT64` / `TIMESTAMP_NS`): `DELTA_BINARY_PACKED`
  - Float64 metric values (`DOUBLE`): `BYTE_STREAM_SPLIT` + `ZSTD`
  - Strings (`service_name`, `metric_name`, `span_name`, `region`): `RLE_DICTIONARY`
  - Trace IDs (`FIXED_SIZE_BYTE_ARRAY(16)`): Parquet native **Split-Block Bloom Filter (`SBBF`)** with False Positive Probability $\text{FPP} = 0.01$.

---

### 4.3 Traces Physical Tables (2 Tables)

#### Table 1: `lake.main.traces_spans` (Raw Span Store)
- **Sort Order:** `ORDER BY (region, service_name, trace_id, start_time_ns)`
  - *Rationale:* Co-locating all spans of the same `trace_id` inside the same Row Group guarantees that once the Bloom Filter matches a Row Group, **1 HTTP Range Request** fetches the entire regional trace segment.
- **Bloom Filter:** Enabled on `trace_id`.

| Column | Parquet / DuckDB Type | Encoding | Description |
| :--- | :--- | :--- | :--- |
| `region` | `VARCHAR` | `RLE_DICTIONARY` | Ingester region (`us-east`, `eu-west`, etc.) |
| `trace_id` | `FIXED_SIZE_BYTE_ARRAY(16)` | `ZSTD` + **Bloom Filter** | 128-bit W3C Trace ID |
| `span_id` | `FIXED_SIZE_BYTE_ARRAY(8)` | `ZSTD` | 64-bit Span ID |
| `parent_span_id` | `FIXED_SIZE_BYTE_ARRAY(8)` | `ZSTD` (Nullable) | `NULL` for root spans |
| `start_time_ns` | `TIMESTAMP_NS` | `DELTA_BINARY_PACKED` | Span start timestamp (nanoseconds) |
| `duration_ns` | `UBIGINT` | `ZSTD` | Span duration in nanoseconds |
| `service_name` | `VARCHAR` | `RLE_DICTIONARY` | OTel `service.name` |
| `span_name` | `VARCHAR` | `RLE_DICTIONARY` | Operation / route name |
| `span_kind` | `UTINYINT` | `RLE_DICTIONARY` | `1=Internal, 2=Server, 3=Client, 4=Producer, 5=Consumer` |
| `status_code` | `UTINYINT` | `RLE_DICTIONARY` | `0=Unset, 1=Ok, 2=Error` |
| `status_message` | `VARCHAR` | `ZSTD` (Nullable) | Error message string |
| `http_status_code` | `USMALLINT` | `ZSTD` (Nullable) | Promoted semantic attribute for fast `5xx` filtering |
| `resource_attrs` | `MAP(VARCHAR, VARCHAR)` | `ZSTD` | Remaining resource attributes |
| `span_attrs` | `MAP(VARCHAR, VARCHAR)` | `ZSTD` | Dynamic span attributes |
| `events` | `LIST(STRUCT(ts TIMESTAMP_NS, name VARCHAR, attrs MAP(VARCHAR, VARCHAR)))` | `ZSTD` | Exceptions & span logs |
| `links` | `LIST(STRUCT(trace_id BLOB, span_id BLOB, attrs MAP(VARCHAR, VARCHAR)))` | `ZSTD` | Async causal links across traces (Kafka/SQS/Batch) |

#### Table 2: `lake.main.traces_index` (Commutative Partial-Segment Trace Summary)
10x smaller index emitted alongside `traces_spans`. Uses **Commutative Partial-Segment Semantics** so traces split across 60s flush boundaries or across multiple global regions merge cleanly via `GROUP BY trace_id`:
- **Sort Order:** `ORDER BY (region, root_service_name, min_start_ns, trace_id)`
- **Bloom Filter:** Enabled on `trace_id` and `linked_trace_ids`.

| Column | Type | Description |
| :--- | :--- | :--- |
| `trace_id` | `FIXED_SIZE_BYTE_ARRAY(16)` | Primary 128-bit trace identifier |
| `region` | `VARCHAR` | Region where this trace segment was ingested |
| `root_service_name` | `VARCHAR` (Nullable) | Populated if `parent_span_id IS NULL` was seen in this segment |
| `root_span_name` | `VARCHAR` (Nullable) | Populated if root span was seen in this segment |
| `min_start_ns` | `TIMESTAMP_NS` | Earliest `start_time_ns` in this segment |
| `max_end_ns` | `TIMESTAMP_NS` | Latest `start_time_ns + duration_ns` in this segment |
| `partial_span_count` | `UINTEGER` | Span count in this segment (`SUM` at query/compaction time) |
| `partial_error_count` | `UINTEGER` | Count of spans with `status_code = 2` in this segment |
| `services_touched` | `LIST(VARCHAR)` | Distinct services in this segment (`list_distinct(flatten(LIST(...)))`) |
| `linked_trace_ids` | `LIST(FIXED_SIZE_BYTE_ARRAY(16))` | Flattened array of linked trace IDs for graph traversal |

---

### 4.4 Metrics Physical Tables (4 Tables)

#### Table 1: `lake.main.metrics_series` (Series Catalog / Dimension Table)
- **Region-Scoped Disjoint `series_id` Formula:**
  $$\text{series\_id} = \text{xxHash64}(\text{region} + \text{"\textbackslash x00"} + \text{instance\_id} + \text{"\textbackslash x00"} + \text{metric\_name} + \text{"\textbackslash x00"} + \text{sorted\_canonical\_labels})$$
  *Why:* Prevents active-active services with identical pod names in two regions (`us-east` vs `eu-west`) from colliding on the same `series_id` and causing false counter resets.

| Column | Type | Description |
| :--- | :--- | :--- |
| `series_id` | `UBIGINT` | Deterministic 64-bit xxHash64 identifier |
| `region` | `VARCHAR` | Region identifier |
| `metric_name` | `VARCHAR` | e.g., `http_requests_total` |
| `metric_type` | `UTINYINT` | `1=Gauge, 2=Sum(Counter), 3=Histogram, 4=ExpHistogram` |
| `temporality` | `UTINYINT` | `1=Delta, 2=Cumulative` |
| `is_monotonic` | `BOOLEAN` | `true` for standard monotonic counters |
| `service_name` | `VARCHAR` | Extracted `service.name` or Prometheus `job` |
| `labels` | `MAP(VARCHAR, VARCHAR)` | Full key-value label map |
| `first_seen_ms` | `BIGINT` | First sample timestamp seen (ms) |
| `last_seen_ms` | `BIGINT` | Updated periodically for active series pruning |

#### Table 2: `lake.main.metrics_points_raw` (High-Resolution Scalar Samples)
- **Sort Order:** `ORDER BY (region, metric_name, series_id, timestamp_ms)`

| Column | Type | Encoding | Description |
| :--- | :--- | :--- | :--- |
| `region` | `VARCHAR` | `RLE_DICTIONARY` | Region identifier for Zone Map pruning |
| `metric_name` | `VARCHAR` | `RLE_DICTIONARY` | Metric name for Zone Map pruning |
| `series_id` | `UBIGINT` | `DELTA_BINARY_PACKED` | Foreign key to `metrics_series` |
| `timestamp_ms` | `BIGINT` | `DELTA_BINARY_PACKED` | Sample timestamp in milliseconds |
| `value` | `DOUBLE` | `BYTE_STREAM_SPLIT` + `ZSTD` | Raw float64 value (used by Gauges) |
| `positive_delta` | `DOUBLE` | `BYTE_STREAM_SPLIT` + `ZSTD` | Exact monotonic increase since previous sample (used by Counters) |
| `is_reset` | `BOOLEAN` | `RLE` | `true` if counter reset occurred (`value < prev_value`) |
| `exemplar_trace_id` | `FIXED_SIZE_BYTE_ARRAY(16)` | `ZSTD` (Nullable) | Links metric spike directly to a W3C Trace ID |

#### Table 3: `lake.main.metrics_histograms_raw` (Native & Exponential Histograms)
Stored separately from scalar points so `metrics_points_raw` stays narrow and SIMD-aligned:
- **Sort Order:** `ORDER BY (region, metric_name, series_id, timestamp_ms)`
- **Columns:** `region VARCHAR`, `metric_name VARCHAR`, `series_id UBIGINT`, `timestamp_ms BIGINT`, `count_delta UBIGINT`, `sum_delta DOUBLE`, `min_val DOUBLE`, `max_val DOUBLE`, `scale TINYINT`, `zero_count UBIGINT`, `positive_buckets LIST(UBIGINT)`, `explicit_bounds LIST(DOUBLE)`, `tdigest_sketch BLOB` (serialized DuckDB T-Digest state for instant associative percentile merging).

#### Table 4: `lake.main.metrics_rollup_5m` & `lake.main.metrics_rollup_1h` (Pre-Aggregated Rollups)
Computed **in-memory during ingestion** (zero S3 read-back cost) and flushed at 5m and 1h boundaries:
- **Columns:** `region VARCHAR`, `metric_name VARCHAR`, `series_id UBIGINT`, `bucket_start_ms BIGINT`, `sample_count UINTEGER`, `min_val DOUBLE`, `max_val DOUBLE`, `sum_val DOUBLE`, `first_ts_ms BIGINT`, `last_ts_ms BIGINT`, `first_val DOUBLE`, `last_val DOUBLE`, `counter_increase DOUBLE`, `reset_count UINTEGER`, `tdigest_sketch BLOB`.

---

### 4.5 Logs Physical Table (`lake.main.logs_records` with Semantic `pattern_hash`)
Supports both OpenTelemetry Logs and Grafana Loki streams without schema drift crashes:
- **Sort Order:** `ORDER BY (region, service_name, severity_number, timestamp_ns)`
- **Ingest-Time Pattern Clustering (Drain3-style):** Before flushing to Parquet, the ingester strips dynamic numbers, UUIDs, IP addresses, and hex strings from `body` to compute a 64-bit `pattern_hash` and `pattern_template` (e.g., `"User <*> failed checkout with code <*>"`).

| Column | Type | Encoding | Description |
| :--- | :--- | :--- | :--- |
| `timestamp_ns` | `TIMESTAMP_NS` | `DELTA_BINARY_PACKED` | Log timestamp |
| `region` | `VARCHAR` | `RLE_DICTIONARY` | Region identifier |
| `service_name` | `VARCHAR` | `RLE_DICTIONARY` | OTel `service.name` or Loki `app`/`job` |
| `severity_text` | `VARCHAR` | `RLE_DICTIONARY` | `INFO`, `WARN`, `ERROR`, `FATAL` |
| `severity_number` | `UTINYINT` | `RLE_DICTIONARY` | OTel severity number (`1`–`24`) |
| `pattern_hash` | `UBIGINT` | `ZSTD` | 64-bit hash of normalized log template |
| `pattern_template`| `VARCHAR` | `RLE_DICTIONARY` | Normalized log template string |
| `body` | `VARCHAR` | `ZSTD` | Full raw log line or message |
| `trace_id` | `FIXED_SIZE_BYTE_ARRAY(16)` | `ZSTD` (Nullable) + **Bloom Filter** | Correlated Trace ID |
| `span_id` | `FIXED_SIZE_BYTE_ARRAY(8)` | `ZSTD` (Nullable) | Correlated Span ID |
| `resource_attrs` | `MAP(VARCHAR, VARCHAR)` | `ZSTD` | Resource/stream labels |
| `log_attrs` | `MAP(VARCHAR, VARCHAR)` | `ZSTD` | Arbitrary structured JSON fields |

---

## 5. Deep Dive: Write Lane, Distributed Ingress & Multi-Region Isolation

```text
OTLP (:4317/:4318) / Prom Remote-Write / Prom Scraper / Loki Push
       │
       ▼
┌─────────────────────────────────────────────────────────────────────────┐
│ 1. Intra-Region Consistent Hash Router (`JumpConsistentHash`)           │
│    - Routes `series_id` / `trace_id` to owner node within SAME region   │
│    - Zero cross-region WAN forwarding                                   │
└───────────────────────────────────┬─────────────────────────────────────┘
                                    ▼
┌─────────────────────────────────────────────────────────────────────────┐
│ 2. Bounded Arrow Memory Arena (Two-Stage Backpressure Valve)            │
│    - Dedupes 60s window `(series_id, timestamp_ms)`                     │
│    - Applies `series_last_point` boundary delta (`positive_delta`)      │
│    - Assigns monotonic `batch_seq_id` for MVCC hot-tail reads           │
│    - Spills frozen ZSTD Arrow IPC to `/tmp` at 75% RAM; 429 at 95% RAM  │
└───────────────────────────────────┬─────────────────────────────────────┘
                                    ▼
┌─────────────────────────────────────────────────────────────────────────┐
│ 3. Parallel S3 Parquet Flush + Lock-Free Commit Envelope                │
│    - Sorts RecordBatch -> Writes ZSTD Parquet + Bloom Filters to S3     │
│    - PUTs 1KB `commits/<region>-<ulid>.json` (`If-None-Match: *`)       │
└─────────────────────────────────────────────────────────────────────────┘
```

### 5.1 Key Write-Lane Guarantees
1. **Boundary-Linked `series_last_point` State Map:** Each owner node maintains a compact hash map `HashMap<u64, (i64, f64)>` (`series_id -> (last_ts_ms, last_raw_val)`, costing just $16\text{MB}$ RAM for $1,000,000$ active series). When Batch $N$ flushes, the first sample of Batch $N$ is compared against Batch $N-1$'s `last_raw_val` so **zero counter increments are lost across 60-second flush boundaries**. On pod startup, this map is hydrated in $< 50\text{ms}$ from the DuckLake catalog checkpoint.
2. **Local RoaringBitmap Series Cache:** Each node tracks known `series_id`s in memory and only writes *newly discovered* series to `metrics/series/ser-<ulid>.parquet`, eliminating $99.9\%$ of catalog metadata write amplification.
3. **Built-In L0 $\rightarrow$ L1 Hourly Compactor & TTL Sweeper:** A background coroutine periodically merges 60-second L0 Parquet files into $128\text{MB}$–$256\text{MB}$ L1 hourly files (collapsing partial `traces_index` segments and maintaining global sort order) and purges expired S3 files according to retention policies.

---

## 6. Deep Dive: Read Lane, Incremental Materialization & PromQL Compatibility

### 6.1 How the Storage Layer Serves Both Query Modes Simultaneously

| Mode | Trigger | How DuckLake + DuckDB Executes It | Latency & S3 Cost |
| :--- | :--- | :--- | :--- |
| **Mode 1: Ad-Hoc / One-Time Query** *(AI Agent investigating an incident, or human exploring UI)* | Arbitrary TQS v2 JSON, Trace ID lookup, PromQL, or SQL | Queries `traces_spans`, `metrics_points_raw`, or `metrics_rollup_5m` directly. Prunes non-matching regions and time windows via DuckLake Zone Maps + Parquet Bloom filters, and unions with MVCC-watermarked hot-tail memory buffers. | **$40\text{ms}$ – $300\text{ms}$** (Reads only matching S3 Parquet row groups) |
| **Mode 2: Defined Dashboard / Alert** *(Fixed query running continuously)* | Saved TQS v2 Dashboard Panel or Alert Rule | Reads from `lake.views.<panel_id>`. Updated every 5s–10s via `ducklake_table_insertions()` using **DuckLake Data Inlining** (`data_inlining_row_limit = 10000`) and Shared-Scan delta evaluation. | **$< 1\text{ms}$** (Hits inlined catalog rows—**Zero S3 GET/PUT calls!**) |

### 6.2 Shared-Scan Incremental Engine & Merge-On-Read Views
1. **Single-Pass Shared Delta Scan:** Every 5 seconds when the Checkpointer advances `snapshot_id`, the Materializer loads `ducklake_table_insertions(last_snap, curr_snap)` **once** into a temporary in-memory DuckDB table (`temp_delta_metrics`, `temp_delta_traces`) and updates all active dashboard views from RAM in a single pass.
2. **Associative Merge-On-Read Semantics:** Materialized tables store additive partial state columns (`sum_delta`, `sample_count`, `min_val`, `max_val`, `tdigest_sketch`) keyed by `(bucket_ts, region, group_by_dims...)`. If two 30-second flushes or late-arriving regional batches land in the same 1-minute `bucket_ts`, reading the dashboard runs a $< 0.5\text{ms}$ `GROUP BY bucket_ts` over the inlined rows and evaluates ratio `formulas[]` last.
3. **Demand-Driven Lease Tiering:** Dashboards actively viewed in the last 30 minutes (plus all Alert Rules) tick continuously every 5s. Cold dashboards pause background ticking and perform a single batch catch-up query ($< 50\text{ms}$) when opened.

### 6.3 The PromQL Compatibility Lane (`ASOF JOIN` + Adaptive Rollup Routing)
To support existing Grafana Prometheus data sources and third-party tooling out of the box, the binary exposes `/api/v1/query`, `/api/v1/query_range`, `/api/v1/series`, and `/api/v1/labels` using a **5-Stage PromQL-to-DuckDB Pipeline**:
1. **Stage 1 (AST Parser):** Parses PromQL via `promql-parser` (Rust) or `prometheus/promql/parser` (Go).
2. **Stage 2 (Series Resolution):** Resolves label matchers (`=`, `!=`, `=~`, `!~`) against `lake.main.metrics_series`.
3. **Stage 3 (Adaptive Tier Router):**
   - Query range $\le 6\text{h}$ or `step < 5m` $\rightarrow$ Routes to `metrics_points_raw` + Hot-Tail Buffer.
   - Query range $6\text{h}\text{–}7\text{d}$ and `step >= 5m` $\rightarrow$ Routes to `metrics_rollup_5m`.
   - Query range $> 7\text{d}$ and `step >= 1h` $\rightarrow$ Routes to `metrics_rollup_1h`.
4. **Stage 4 (Vectorized DuckDB `ASOF JOIN` & Window Compilation):**
   - **Gauges & Instant Queries (`5m` Lookback Delta):** Uses DuckDB's native vectorized `ASOF JOIN` to find the most recent sample at or before each evaluation step timestamp within Prometheus's 5-minute staleness lookback window:
     ```sql
     WITH step_grid AS (
         SELECT unnest(generate_series($start_ms, $end_ms, $step_ms)) AS eval_ts
     )
     SELECT g.eval_ts, s.region, s.service_name, p.value
     FROM step_grid g
     CROSS JOIN matched_series s
     ASOF JOIN lake.main.metrics_points_raw p
       ON p.series_id = s.series_id
      AND p.timestamp_ms <= g.eval_ts
     WHERE p.timestamp_ms >= g.eval_ts - 300000; -- Exact 5m PromQL lookback delta
     ```
   - **Counters (`rate(v[5m])` / `increase(v[5m])`):** Sums `positive_delta` inside `[eval_ts - range_ms, eval_ts]` across each `series_id`, divides by window seconds for `rate()`, and applies outer `sum by (...)` / `avg by (...)` groupings.
5. **Stage 5 (Arrow-to-Prometheus Formatter):** Streams Arrow `RecordBatch` results into standard Prometheus `matrix` / `vector` JSON responses.

---

## 7. Telemetry Query Spec (TQS v2) Formal JSON Schema & Compiler

While PromQL is supported for legacy compatibility, our primary AI-first interface for dashboards, alerts, and agent investigations is **TQS v2**—a strict declarative JSON schema that eliminates LLM syntax hallucinations and compiles deterministically into both Ad-Hoc scans and Incremental Materialized Views.

### 7.1 Formal JSON Schema (`tqs_v2.schema.json`)

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "$id": "https://fanout-lake.dev/schemas/tqs-v2.schema.json",
  "title": "TelemetryQuerySpecV2",
  "description": "Declarative multi-region query specification for DuckLake-backed metrics, traces, and logs",
  "type": "object",
  "required": ["id", "signal", "target", "time_range"],
  "properties": {
    "id": {
      "type": "string",
      "pattern": "^[a-z0-9_-]+$",
      "description": "Unique identifier / slug for the query or dashboard panel"
    },
    "title": {
      "type": "string",
      "description": "Human-readable title for UI panels and reports"
    },
    "signal": {
      "type": "string",
      "enum": ["metrics", "traces", "logs"],
      "description": "Primary telemetry signal to query"
    },
    "target": {
      "type": "string",
      "description": "For metrics: metric_name (e.g. 'http_requests_total'). For traces: 'spans', 'traces_summary', 'trace_waterfall', 'span_links'. For logs: 'records' or 'patterns'."
    },
    "trace_id": {
      "type": "string",
      "pattern": "^[0-9a-fA-F]{32}$",
      "description": "32-character hex W3C Trace ID (required when target is 'trace_waterfall')"
    },
    "time_range": {
      "type": "object",
      "required": ["start"],
      "properties": {
        "start": { "type": "string", "description": "Relative duration ('15m', '1h', '7d') or ISO-8601 timestamp" },
        "end": { "type": "string", "default": "now", "description": "Relative duration or ISO-8601 timestamp" }
      }
    },
    "step": {
      "type": "string",
      "pattern": "^[0-9]+(s|m|h|d)$",
      "description": "Time bucket resolution (e.g. '10s', '1m', '5m', '1h')"
    },
    "filter": {
      "type": "object",
      "description": "Global dimension/attribute filters (including 'region', 'service_name', 'span_attrs.*', 'labels.*')",
      "additionalProperties": {
        "oneOf": [
          { "type": ["string", "number", "boolean"] },
          {
            "type": "object",
            "properties": {
              "eq": { "type": ["string", "number", "boolean"] },
              "neq": { "type": ["string", "number", "boolean"] },
              "gt": { "type": "number" },
              "gte": { "type": "number" },
              "lt": { "type": "number" },
              "lte": { "type": "number" },
              "in": { "type": "array", "items": { "type": ["string", "number"] } },
              "regex": { "type": "string" },
              "exists": { "type": "boolean" }
            },
            "additionalProperties": false
          }
        ]
      }
    },
    "group_by": {
      "type": "array",
      "items": { "type": "string" },
      "description": "Dimensions to group by (e.g. ['region', 'service_name', 'http_status_code'])"
    },
    "aggregations": {
      "type": "array",
      "items": {
        "type": "object",
        "required": ["op", "as"],
        "properties": {
          "op": {
            "type": "string",
            "enum": [
              "counter_rate",
              "counter_increase",
              "gauge_avg",
              "gauge_max",
              "gauge_min",
              "gauge_last",
              "sum",
              "count",
              "error_count",
              "p50",
              "p90",
              "p95",
              "p99"
            ]
          },
          "field": {
            "type": "string",
            "default": "positive_delta",
            "description": "Column to aggregate ('positive_delta', 'value', 'duration_ns')"
          },
          "filter": {
            "type": "object",
            "description": "Optional per-aggregation conditional filter compiled to SQL FILTER (WHERE ...)"
          },
          "as": {
            "type": "string",
            "pattern": "^[a-z0-9_]+$"
          }
        }
      }
    },
    "formulas": {
      "type": "array",
      "description": "Post-aggregation arithmetic evaluated AFTER Merge-On-Read SUM() so ratios across partial buckets and regions are 100% exact",
      "items": {
        "type": "object",
        "required": ["as", "expr"],
        "properties": {
          "as": { "type": "string", "pattern": "^[a-z0-9_]+$" },
          "expr": { "type": "string", "description": "Arithmetic expression referencing aggregation aliases, e.g. '(errors_5xx / NULLIF(total_reqs, 0)) * 100.0'" }
        }
      }
    },
    "order_by": {
      "type": "object",
      "properties": {
        "field": { "type": "string" },
        "direction": { "type": "string", "enum": ["asc", "desc"], "default": "desc" }
      }
    },
    "limit": {
      "type": "integer",
      "minimum": 1,
      "maximum": 5000,
      "default": 100
    }
  }
}
```

### 7.2 Concrete TQS v2 Examples

#### Example 1: Global Multi-Region Error Percentage Ratio (Uses Conditional Filter + `formulas[]`)
```json
{
  "id": "global_checkout_error_pct",
  "title": "Global Checkout 5xx Error % by Region",
  "signal": "metrics",
  "target": "http_requests_total",
  "time_range": { "start": "1h", "end": "now" },
  "step": "1m",
  "filter": { "service_name": "checkout-api" },
  "group_by": ["region", "service_name"],
  "aggregations": [
    {
      "op": "counter_increase",
      "field": "positive_delta",
      "as": "errors_5xx",
      "filter": { "http_status_code": { "gte": "500" } }
    },
    {
      "op": "counter_increase",
      "field": "positive_delta",
      "as": "total_requests"
    }
  ],
  "formulas": [
    {
      "as": "error_rate_pct",
      "expr": "(errors_5xx / NULLIF(total_requests, 0)) * 100.0"
    }
  ]
}
```

#### Example 2: Trace-to-Metrics RED Materialization (Latency p95 & Error Rate from Spans)
```json
{
  "id": "checkout_span_red_metrics",
  "title": "Checkout Span p95 Latency & Failures",
  "signal": "traces",
  "target": "spans",
  "time_range": { "start": "6h", "end": "now" },
  "step": "1m",
  "filter": {
    "service_name": "checkout-api",
    "span_kind": 2
  },
  "group_by": ["region", "span_name"],
  "aggregations": [
    { "op": "count", "as": "total_spans" },
    { "op": "error_count", "as": "failed_spans", "filter": { "status_code": 2 } },
    { "op": "p95", "field": "duration_ns", "as": "p95_duration_ns" }
  ],
  "formulas": [
    {
      "as": "span_error_pct",
      "expr": "(failed_spans / NULLIF(total_spans, 0)) * 100.0"
    }
  ]
}
```

---

## 8. AI-First Architecture & Model Context Protocol (MCP) Server

### 8.1 Zero-Load Local Laptop Investigation Mode
Because S3/R2 + DuckLake is an open, self-describing storage format, an engineer or background AI agent can run our binary locally on their laptop:
```bash
myengine mcp --s3-bucket s3://prod-telemetry --read-only
```
The laptop binary attaches `s3://prod-telemetry/catalog/snapshot-latest.ducklake` in read-only mode and executes DuckDB queries on the laptop's CPU using HTTP Range reads against S3/R2—**consuming $0\%$ CPU and $0\text{MB}$ RAM on production servers**.

### 8.2 Complete MCP Tool Specifications (5 Core Tools)

```json
[
  {
    "name": "discover_telemetry_catalog",
    "description": "Lists active regions, services, metric names, span operations, log patterns, and dynamic attribute keys present in the DuckLake catalog. Always call this first before constructing queries so you never hallucinate metric or attribute names.",
    "inputSchema": {
      "type": "object",
      "properties": {
        "signal": { "type": "string", "enum": ["metrics", "traces", "logs", "all"], "default": "all" },
        "region": { "type": "string", "description": "Optional: filter discovery to a specific region (e.g. 'us-east')" },
        "service_name": { "type": "string", "description": "Optional: filter discovery to a specific service" },
        "lookback": { "type": "string", "default": "6h" }
      }
    }
  },
  {
    "name": "run_telemetry_query",
    "description": "Executes an ad-hoc TQS v2 query across S3 Parquet files and live hot-tail buffers using DuckDB. Returns token-efficient Markdown tables or time-series matrices.",
    "inputSchema": {
      "type": "object",
      "required": ["query"],
      "properties": {
        "query": { "$ref": "https://fanout-lake.dev/schemas/tqs-v2.schema.json" },
        "response_format": {
          "type": "string",
          "enum": ["compact_table", "json_records", "time_series_matrix"],
          "default": "compact_table"
        }
      }
    }
  },
  {
    "name": "get_trace_waterfall",
    "description": "Reconstructs an end-to-end distributed trace execution tree across all regions by trace_id. Returns an indented call hierarchy with timing, self-time, region hops, error stack traces, and async SpanLinks.",
    "inputSchema": {
      "type": "object",
      "required": ["trace_id"],
      "properties": {
        "trace_id": { "type": "string", "pattern": "^[0-9a-fA-F]{32}$" },
        "include_events": { "type": "boolean", "default": true },
        "include_correlated_logs": { "type": "boolean", "default": true }
      }
    }
  },
  {
    "name": "correlate_anomalies",
    "description": "Compares an incident time window against a baseline window using DuckDB statistical functions. Automatically surfaces which region, pod, attribute key-value, or new Drain3 log pattern_hash explains the anomaly spike.",
    "inputSchema": {
      "type": "object",
      "required": ["service_name", "incident_start", "incident_end", "baseline_start", "baseline_end"],
      "properties": {
        "service_name": { "type": "string" },
        "incident_start": { "type": "string" },
        "incident_end": { "type": "string" },
        "baseline_start": { "type": "string" },
        "baseline_end": { "type": "string" },
        "signal_focus": { "type": "string", "enum": ["traces", "metrics", "logs", "all"], "default": "all" }
      }
    }
  },
  {
    "name": "save_materialized_dashboard_panel",
    "description": "Registers a validated TQS v2 query as a continuously maintained dashboard panel backed by DuckLake inlined incremental storage.",
    "inputSchema": {
      "type": "object",
      "required": ["dashboard_slug", "panel_id", "query"],
      "properties": {
        "dashboard_slug": { "type": "string" },
        "panel_id": { "type": "string", "pattern": "^[a-z0-9_-]+$" },
        "query": { "$ref": "https://fanout-lake.dev/schemas/tqs-v2.schema.json" },
        "refresh_interval": { "type": "string", "default": "10s" }
      }
    }
  }
]
```

---

## 9. Zero-Friction Drop-In Migration, Historical Import/Export & Web RUM

### 9.1 5-Minute Drop-In Migration for Existing Stacks
1. **Existing Prometheus Users (Push or Pull):**
   - **Option A (`remote_write`):** Add 3 lines to `prometheus.yml`:
     ```yaml
     remote_write:
       - url: "http://myengine:4318/api/v1/write"
     ```
   - **Option B (Native Scrape Cutover):** Pass `--prometheus-config=prometheus.yml` directly to our binary (supports `relabel_configs` and Kubernetes service discovery) and turn off the heavy Prometheus server.
2. **Existing OpenTelemetry Users:** Point `OTEL_EXPORTER_OTLP_ENDPOINT=http://myengine:4317` (gRPC) or `:4318` (HTTP).
3. **Existing Grafana Loki Users:** Point Promtail, FluentBit, Vector, or Grafana Alloy to `http://myengine:4318/loki/api/v1/push`.
4. **AI Grafana Dashboard Migrator (`myengine import-grafana`):**
   - Reads existing Grafana Dashboard JSON exports, translates PromQL/LogQL panels into declarative **TQS v2** specs (automatically converting error ratio queries into `formulas[]`), and runs **Shadow Validation (`--validate-migration`)** against the live Prometheus endpoint to verify $< 1\%$ Mean Squared Error divergence before cutover.

### 9.2 Historical Prometheus TSDB & Loki Bulk Import/Export
- **`myengine import prometheus --tsdb-path /prometheus/data --s3 s3://bucket`:** Reads local Prometheus 2-hour block files (`chunks/` + `index`), computes `series_id` and `positive_delta`, and writes sorted ZSTD Parquet files directly to S3 + DuckLake so teams keep years of historical metrics on Day 1.
- **`myengine import loki --chunks-dir /loki/chunks --s3 s3://bucket`:** Decodes historical Loki compressed chunks, extracts Drain3 `pattern_hash`, and writes sorted `logs_records` Parquet files to S3.
- **Zero Lock-In Export:** Because all data lives in standard Apache Parquet on S3, external Spark, Trino, ClickHouse, or Python Polars jobs can read the files directly, or export slices via `myengine export prometheus`.

### 9.3 Frontend Telemetry (Web RUM & Sourcemap Unminification)
Our foundation extends to Browser Real User Monitoring (RUM) without adding a separate database:
- **Public `/v1/rum` Ingestion Endpoint:** Accepts lightweight browser OTLP payloads with CORS, rate-limiting, and PII scrubbing.
- **Core Web Vitals & Session Stitching:** Browser `LCP`, `CLS`, `INP` metrics flow into `metrics_histograms_raw`, while browser fetch/XHR spans carrying W3C `traceparent` headers flow into `traces_spans` with `span_attrs['session.id']`. Calling `get_trace_waterfall(trace_id)` shows the complete trace starting from the user's React button click in Chrome, across `us-east` API Gateway, down to the `eu-west` PostgreSQL query!
- **S3 Sourcemap Resolver:** CI pipelines upload minified JS sourcemaps to `s3://<bucket>/rum/sourcemaps/service=<app>/release=<sha>/`. When a browser error span arrives, the ingester caches the release sourcemap in memory and writes the unminified TypeScript file/line stack trace directly into `traces_spans.events`.

---

## 10. Architectural Stress-Test Matrix & Verification Suite

All **20 architectural failure modes** across Write, Read, Multi-Region, Storage, and Migration layers have been hardened in this design and scheduled for automated chaos verification:

| # | Layer | Failure Mode / Hole | Why Naive Designs Fail | Hardened v5 Engineering Solution |
| :--- | :--- | :--- | :--- | :--- |
| **1** | **Write** | **Intra-Region Load Balancer Spray** | Round-robin LB sprays samples of the same `series_id` or `trace_id` across $N$ nodes, ruining Parquet compression and local delta tracking. | **Consistent Hash Peer Routing (Intra-Region):** Nodes shard streams by `JumpConsistentHash(series_id)` over persistent VPC gRPC streams. |
| **2** | **Write** | **S3 Catalog Commit Contention** | 20 stateless ingesters flushing simultaneously trigger `412 Precondition Failed` retry storms on S3 catalog files. | **Lock-Free S3 Commit Queue:** Nodes write `commits/<region>-<ulid>.json` via `If-None-Match: *`; Checkpointer batches them every 5s. |
| **3** | **Write** | **Series Metadata Write Amplification** | Re-inserting existing `metrics_series` definitions on every batch floods the catalog DB. | **Local RoaringBitmap / LRU Series Cache:** Nodes cache known `series_id`s ($8\text{MB}$ per $1\text{M}$ series) and only emit newly discovered series. |
| **4** | **Write** | **Traffic Burst / S3 Outage OOM** | If S3 slows down during a traffic spike, unbounded in-memory buffers trigger Kubernetes `OOMKilled`. | **Bounded Arrow Arena + Two-Stage Backpressure:** Spills frozen ZSTD batches to `/tmp` at $75\%$ RAM; returns `429 / RESOURCE_EXHAUSTED` at $95\%$ RAM. |
| **5** | **Write** | **Boundary Sample Gap Across Flushes** | First sample in Flush $N$ needs the last sample from Flush $N-1$ to compute `positive_delta`; otherwise 1 interval per minute is lost. | **Boundary-Linked `series_last_point` State:** Owner nodes keep `(last_ts, last_val)` per active series across flushes, hydrated on startup. |
| **6** | **Read** | **Hot-Tail Flush Duplication Race** | A batch committed to DuckLake is still in RAM for $2\text{ms}$, causing `UNION ALL` queries to double-count that batch. | **MVCC `batch_seq_id` Watermarking:** Hot-tail Arrow scan filters `WHERE batch_seq_id > snapshot.max_committed_seq_id`. |
| **7** | **Read** | **Ratios & Error % in Incremental Views** | Pre-computing `5xx / total` per flush or per region is broken because ratios cannot be summed or averaged. | **TQS v2 Conditional Aggregations + Merge-On-Read `formulas[]`:** Stores additive `errors_5xx` and `total_requests` in view; divides at read time after `SUM()`. |
| **8** | **Read** | **1,000 Dashboards CPU Storm** | Running 1,000 separate `ducklake_table_insertions` queries every 10s saturates CPU and catalog I/O. | **Shared-Scan Delta Table + Lazy Lease Tiering:** Loads delta rows once into RAM per tick for all views; pauses cold dashboards until opened. |
| **9** | **Read** | **Split Traces Across 60s Boundaries** | A long trace spans two 60s flushes; naive indexers write incomplete duration or lose `root_service_name`. | **Commutative Partial-Segment `traces_index`:** Stores `min_start_ns`, `max_end_ns`, and `COALESCE(root_service_name)`, merged via `GROUP BY trace_id`. |
| **10** | **Read** | **Late-Arriving OTel Samples** | Mobile/edge SDKs send metrics 45s late, landing in an already-materialized 1m dashboard bucket. | **Associative Merge-On-Read Views:** Late delta rows append a new partial row for that `bucket_ts`; UI read `GROUP BY bucket_ts` merges it in $< 0.5\text{ms}$. |
| **11** | **Multi-Region** | **Cross-Region WAN Write Latency** | Forwarding OTLP packets across global regions via peer hashing adds $200\text{ms}$ latency and huge WAN egress bills. | **Region-Local Write Isolation:** Consistent hash ring is strictly scoped per region; every region flushes directly to S3 with zero WAN peer forwarding. |
| **12** | **Multi-Region** | **Cross-Region Series ID Collision** | Same service (`checkout-api`) in `us-east` and `eu-west` with identical pod names (`pod-0`) collides on `series_id`, causing fake counter resets. | **Region-Scoped `series_id` Fingerprinting:** Prepends `region + instance_id` into `xxHash64` input so series IDs are mathematically disjoint across regions. |
| **13** | **Multi-Region** | **Cross-Region Distributed Trace Split** | Trace starts in `us-east` API Gateway and calls `eu-west` Payment service; spans land in different regional Parquet files. | **Global Bloom-Filtered Trace Stitching:** DuckDB checks `trace_id` Bloom filters across regional partitions in parallel and merges partial `traces_index` rows. |
| **14** | **Multi-Region** | **Regional Clock Skew & Delayed Flush** | `ap-southeast` flushes 15s later than `us-east`, causing global dashboard buckets to look incomplete if sealed early. | **Region-Grain Partial Materialization:** Materialized views store `(bucket_ts, region, ...)`; late regional flushes append cleanly and merge on read. |
| **15** | **Multi-Region** | **WAN Partition During Hot-Tail Query** | `ap-southeast` WAN link drops while a user runs a live global query. | **Bounded-Timeout Federated Hot-Tail (`150ms`):** Reads all regions' committed S3 data + reachable hot-tails; returns partial warning header without failing. |
| **16** | **Storage** | **S3 Small-File Proliferation** | 1,440 flushes/day/region creates $100,000+$ Parquet files/month, slowing down long-range queries. | **Built-In L0 $\rightarrow$ L1 Hourly Compactor & TTL Sweeper:** Merges 1m L0 files per region into $128\text{MB}$–$256\text{MB}$ L1 hourly files and deletes expired partitions. |
| **17** | **Storage** | **Single-Node S3 Catalog Split-Brain** | Two processes accidentally overwrite `snapshot.ducklake` on S3. | **S3 Conditional PUTs (`If-Match: <ETag>`):** Enforces strict optimistic concurrency on catalog snapshots and checkpointer leases. |
| **18** | **Migrate** | **Prometheus High-Cardinality Bomb** | Rogue scrape target emits `user_id` labels, creating millions of series. | **Ingest Cardinality Circuit Breaker:** Per-metric cap (`50,000` active series) + regex `relabel_configs`. |
| **19** | **Migrate** | **HA Prometheus Duplicate Samples** | Two HA Prometheus servers remote-write identical samples with slight clock skew. | **60s Rolling Deduplication & Pre-Flush Sort:** Deduplicates `(series_id, timestamp_ms)` in the ring buffer before delta calculation. |
| **20** | **Migrate** | **Loki Unstructured JSON Drift** | Dynamic JSON log keys crash strict Parquet column schemas. | **Hybrid Fixed + `MAP(VARCHAR, VARCHAR)` Schema:** Core columns fixed; dynamic attributes stored in compressed Parquet `MAP`. |

### 10.1 Automated Chaos & Benchmark Test Suite (Execution Backlog)
1. **Test 1 — Multi-Region Active-Active Counter Accuracy Test:** 3 regional instances (`us-east`, `eu-west`, `ap-southeast`) ingesting identical pod labels at $1,000\text{ req/s/region}$ with mid-run `kill -9` on `eu-west`. Verify $0\%$ error in global `counter_increase`.
2. **Test 2 — Cross-Region Trace Stitching & SpanLink Benchmark:** $10,000$ traces hopping across all 3 regions and spanning 60s flush boundaries. Verify $100\%$ span reconstruction in $< 80\text{ms}$.
3. **Test 3 — S3 Outage & Backpressure Survival Test:** 5-minute S3 blackout via Toxiproxy under $100,000\text{ spans/s}$ flood. Verify strict RSS memory cap, `/tmp` disk spill, and $100\%$ recovery on reconnect.
4. **Test 4 — 1,000 Materialized Dashboard Panels Scale Test:** 1,000 TQS v2 views (with ratio `formulas[]`) across 3 regions. Verify 5s shared-scan checkpoint takes $< 250\text{ms}$ CPU and reads take $< 2\text{ms}$.
5. **Test 5 — MVCC Hot-Tail Transition Consistency Test:** $50\text{ms}$ continuous polling during S3 flush and catalog commit. Verify strictly monotonic results with zero double-counting or dips.

---

## 11. Implementation Roadmap (MVP to v1.0)

- **Phase 1: Core Ingester, Region-Scoped Identity & S3 Parquet Writer (Weeks 1–2)**
  - Build OTLP gRPC/HTTP receiver + Prometheus Remote-Write receiver + Loki Push receiver.
  - Implement region-scoped `series_id` hashing, intra-region Consistent Hash peer routing, bounded Arrow memory arena, `series_last_point` boundary delta calculation, and lock-free S3 Parquet + Commit Envelope flushing.
- **Phase 2: DuckLake Checkpointer, Multi-Region Read & TQS v2 / PromQL Engine (Weeks 3–4)**
  - Implement the 5s DuckLake Catalog Checkpointer + Shared-Scan Incremental Materializer.
  - Integrate `libduckdb` + MVCC-watermarked hot-tail scan + Federated regional hot-tail RPC (`150ms` timeout).
  - Build the TQS v2 compiler (`FILTER WHERE` + Merge-On-Read `formulas[]`) and the 5-stage PromQL `ASOF JOIN` compatibility endpoint.
- **Phase 3: AI MCP Server, Compactor & CLI (Week 5)**
  - Expose all 5 MCP tools over `stdio` and Streamable HTTP, including local laptop `--read-only` S3 mode.
  - Add background L0 $\rightarrow$ L1 hourly Parquet compactor, `correlate_anomalies` statistical query, and AI Grafana-JSON-to-TQS v2 dashboard importer (`--validate-migration`).
- **Phase 4: Historical Importers, Embedded Web UI, Frontend RUM & Chaos Verification (Week 6+)**
  - Ship embedded single-binary Web UI for global/regional dashboards and trace waterfalls.
  - Add historical Prometheus TSDB / Loki bulk importers and `/v1/rum` + S3 Sourcemap unminification.
  - Execute the 5 automated Chaos & Verification Tests in Section 10.1.