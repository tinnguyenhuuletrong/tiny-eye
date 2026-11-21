---
title: Storage format
description: What tiny-eye stores and how, covering the generated tables, Parquet writer settings, object key layout, envelopes, identities, and the DuckDB build and extensions.
sidebar_position: 2
---

tiny-eye keeps data in three places:

| Place | Contents | Lifetime |
| --- | --- | --- |
| Object storage (`storage.url`: a local directory or `s3://bucket/prefix`) | Parquet data files (L0 from flushes, L1 from compaction) and commit envelopes | Data files: until retention. Envelopes: until the next checkpoint applies them |
| DuckLake catalog (`catalog.url`: a DuckDB file or `postgres:<conninfo>`) | Table definitions, the list of registered files with their column statistics, snapshots, and the `applied_envelopes` table | Permanent. Snapshots older than 10 minutes are expired by retention |
| Local WAL directory (`ingest.wal_dir`, `durability: wal` only) | One segment file per flush sequence | Until that sequence's envelope is in object storage |

The in-memory DuckDB database holding the hot tables is not storage. It is rebuilt empty on every start and refilled from the WAL. See [Write path](write-path.md).

## `schema/tables.yaml`: the one table definition

Every stored table is defined once in `schema/tables.yaml`. Writing the definition twice would be a bug that surfaces late. `ducklake_add_data_files` checks a file's schema against the catalog table only at commit time (E3). A hand-written Arrow schema that drifts from the DDL would make the first checkpoint fail, after the data was already acknowledged and flushed.

Each table has columns plus four keys:

| Key | Meaning | Used by |
| --- | --- | --- |
| `sort` | Row order inside every Parquet file | Flush export (`ORDER BY`), compaction, `parquet::sort` |
| `unique` | The identity of a row. Flush and compaction keep one row per identity, so a retried request or a replayed WAL segment never double counts. Empty means every row is distinct | `Table::dedup_sql()` |
| `time` | The column that decides a row's hour partition | `Table::hour_sql()` |
| `bloom` (column option) | Split-block bloom filter, FPP 0.01, dictionary off | Parquet writer |

The four tables:

| Table | Sort | Unique | Time column | Notes |
| --- | --- | --- | --- | --- |
| `metrics_series` | `series_id` | `series_id` | `day_ms` | One row per (series, flush). Readers deduplicate by `series_id` |
| `metrics_points` | `series_id, timestamp_ms` | `series_id, timestamp_ms` | `timestamp_ms` | Raw samples. `timestamp_ms` uses delta encoding and `value` uses byte-stream-split. `positive_delta` is filled by the compactor (D2) |
| `spans` | `trace_id, start_time_ns` | `trace_id, span_id` | `start_time_ns` | `trace_id` has a bloom filter |
| `logs` | `service_name, severity_number, timestamp_ns` | none | `timestamp_ns` | Two identical log lines are two lines. Logs are never deduplicated |

Two consequences of these keys are easy to miss:

- `metrics_series` is partitioned by `day_ms`, which receivers set to midnight UTC of the sample's day. `hour_sql()` for an `i64` column is `col // 3600000`, so every series row of a day lands in that day's `hour=00` partition. The partition is re-compacted whenever new series rows arrive for the day. Retention drops it when hour 00 of that day expires. That is up to 23 hours before the same day's last points expire, so the points of a series that stopped reporting that day become unselectable slightly early, at the very edge of retention.
- Dedup keeps one row per identity, and which row it keeps is unspecified. The window is `row_number() OVER (PARTITION BY <unique>)` with no `ORDER BY`. This is exact for retried duplicates. If two different values share a `(series_id, timestamp_ms)`, one of them survives arbitrarily.

### What `crates/engine/build.rs` generates

The build script reads the YAML with `deny_unknown_fields` and writes `tables.rs` into `OUT_DIR`. `crates/engine/src/tables.rs` includes it. It generates:

- `SCHEMA_VERSION` (the file's `version`) and the `Table` enum with `Table::ALL` (alphabetical: `Logs`, `MetricsPoints`, `MetricsSeries`, `Spans`). The WAL stores the index into `ALL`, so the table set must not change while WAL segments exist.
- Per table: `name()`, `from_name()`, `doc()`, `sort_columns()`, `unique_columns()`, `time_column()`, `column_names()`, `bloom_columns()`, `delta_columns()`, `byte_stream_split_columns()`.
- `dedup_sql()`: `" QUALIFY row_number() OVER (PARTITION BY <unique>) = 1"`, or an empty string.
- `hour_sql()`: `(col // 3600000)` for an `i64` millisecond column, and `(epoch_ns(col) // 3600000000000)` for `timestamp_ns`. The code comment records why it is not `epoch_ms`: DuckDB 2.0.1 raises a spurious cast error for `epoch_ms(TIMESTAMP_NS) // n`.
- `ddl_columns()` and `ddl(name)`: `CREATE TABLE IF NOT EXISTS <name> (...)`, used for `lake.<table>`. The hot tables use `ddl_columns()` plus `_seq BIGINT`.
- `schema()`: the Arrow schema, carrying `schema_version` metadata.
- A `<Table>Row` struct (nullable columns become `Option`) and a `<Table>Builder` with `append(&row)`, `len()`, `finish() -> RecordBatch`.

The build fails if a `sort`, `unique` or `time` entry names an unknown column, if a type is unknown, or if the time column is neither `i64` nor `timestamp_ns`.

Type mapping:

| YAML type | Rust field | Arrow | DuckDB |
| --- | --- | --- | --- |
| `utf8` | `String` | `Utf8` | `VARCHAR` |
| `u8` / `u32` / `u64` | `u8` / `u32` / `u64` | `UInt8` / `UInt32` / `UInt64` | `UTINYINT` / `UINTEGER` / `UBIGINT` |
| `i64` | `i64` | `Int64` | `BIGINT` |
| `f64` | `f64` | `Float64` | `DOUBLE` |
| `bool` | `bool` | `Boolean` | `BOOLEAN` |
| `timestamp_ns` | `i64` | `Timestamp(Nanosecond)` | `TIMESTAMP_NS` |
| `date` | `i32` | `Date32` | `DATE` |
| `trace_id` | `[u8; 16]` | `Binary` | `BLOB` |
| `map` | `Labels` (`BTreeMap<String, String>`) | `Map<Utf8, Utf8>` | `MAP(VARCHAR, VARCHAR)` |
| `utf8_list` | `Vec<String>` | `List<Utf8>` | `VARCHAR[]` |

`trace_id` is a variable-length `Binary` that always holds 16 bytes. The spec proved `FixedSizeBinary(16)` to `BLOB`. The generated `Binary` maps to the same `BLOB`, and PoC P1 and P7 prove it registers and round-trips. `date` and `utf8_list` are supported but unused.

:::warning No schema migration
`ddl()` is `CREATE TABLE IF NOT EXISTS`. Changing a column in `tables.yaml` changes the files the writer produces but not an existing lake table, so `ducklake_add_data_files` will refuse the new files at checkpoint (E3). There is no `ALTER` path yet. A schema change on an existing deployment needs a migration step that has not been written. The `schema_version` value is written into every Parquet footer, but no reader checks it today. Only the envelope `format` is checked.
:::

## Parquet files

`crates/engine/src/parquet.rs` writes every data file, both flush (L0) and compaction (L1). DuckDB is used to sort and deduplicate the rows, then hands back Arrow batches, and our writer encodes them. The spec's research found that DuckDB's own Parquet writer writes no bloom filter on a high-cardinality column like `trace_id` (finding 4, D4). It also cannot choose per-column encodings from our table definition.

| Setting | Value in code | Spec section 4.2 evidence |
| --- | --- | --- |
| Compression | ZSTD level 3 | E2 |
| Statistics | `EnabledStatistics::Page` (page-level statistics, with column and offset index) | E2 |
| Row group | `ingest.row_group_rows`, default 122,880 | U (the research used 4,096 to 8,192) |
| `metrics_points.timestamp_ms` | `DELTA_BINARY_PACKED`, dictionary off | E2 |
| `metrics_points.value` | `BYTE_STREAM_SPLIT`, dictionary off | E2 |
| `spans.trace_id` | Bloom filter on, FPP 0.01, dictionary off | E4. P6 checks a filter in every row group |
| Footer metadata | `schema_version = 1` | spec section 13 |

`parquet::write` takes the Arrow schema from the first batch, which is DuckDB's export of the hot table or of `read_parquet` during compaction. It applies encodings and bloom filters by column name from `tables.yaml`. With no batches it falls back to the generated schema.

The sort order matters for reads. Spans sorted by `trace_id` give tight min/max statistics per row group, and the bloom filter lets DuckDB skip row groups that cannot hold an id. The research measured three layouts for 11 L0 files (spec section 7.2): sorted by `trace_id` with bloom, 26 GETs and 294.7 KiB. Sorted without bloom, 22 GETs and 623.8 KiB. The v5 layout (service first, with bloom), 38 GETs and 509.6 KiB. The first is the chosen layout.

## Object key layout

`store.rs` builds every key (spec section 4.1):

```text
<table>/tenant=<tenant>/date=<YYYY-MM-DD>/hour=<HH>/l0-<ulid>-<node_id>.parquet    flush output
<table>/tenant=<tenant>/date=<YYYY-MM-DD>/hour=<HH>/l1-<ulid>-compacted.parquet    compaction output
commits/<region>/<node_id>-<epoch:13 digits>-<seq:12 digits>.json                  envelopes
lake/                                                                              DuckLake DATA_PATH
```

- Keys are relative to the store root. With S3 the bucket prefix from `s3://bucket/prefix` is added. `Store::url(key)` gives the absolute URL DuckDB reads (`/abs/dir/<key>` or `s3://bucket/prefix/<key>`). That absolute URL is what the envelope and the catalog record (D9: absolute file paths).
- The `date=` and `hour=` parts are computed from the unix hour with a calendar-free civil-from-days function (`date_of_hour`). `hour_of_key` parses them back. Compaction and retention take a file's hour from its key, not from its contents. The unit test `hour_round_trips_through_key` covers both directions.
- DuckLake never parses the hierarchy (E3). It serves humans, lifecycle backstops and audits.
- `lake/` is the `DATA_PATH` passed to `ATTACH 'ducklake:...'`. tiny-eye writes no files there. All our files are registered from their own keys with `ducklake_add_data_files`, and DuckLake later deletes them anyway (E6).
- The envelope key is deterministic per (node, epoch, sequence), and the zero padding makes lexical order equal numeric order. [Write path](write-path.md) explains why it is deterministic.

On S3 the client is built with `S3ConditionalPut::ETagMatch`, so `PutMode::Create` becomes `If-None-Match: *` (E1). On local disk `LocalFileSystem` gives the same create-only semantics. `Store::put_new` returns `true` if it created the object and `false` if the object already existed, and treats `AlreadyExists` and `Precondition` alike.

## Envelopes

An envelope is the only thing that makes a flush's files visible once applied (spec section 5.3). It is a JSON object (`node::Envelope`):

```json
{ "format": 1, "envelope_id": "01JC...", "kind": "ingest", "tenant": "default", "region": "local",
  "node_id": "node-0", "node_epoch": 1762740000000, "seq_to": 4,
  "files": [ { "table": "metrics_points", "path": "/abs/data/objects/metrics_points/tenant=default/date=2025-11-10/hour=02/l0-01JC...-node-0.parquet", "bytes": 241311, "rows": 52400 } ] }
```

`node_epoch` is the process start time in milliseconds, so it changes on every start. `seq_to` restarts at 1 in each epoch. The checkpointer refuses any envelope whose `format` is not 1 and names both versions.

## Catalog tables

`Node::open` attaches the catalog as `lake` and creates:

- `lake.metrics_series`, `lake.metrics_points`, `lake.spans`, `lake.logs` from the generated DDL;
- `lake.applied_envelopes (envelope_id VARCHAR, region VARCHAR, node_id VARCHAR, node_epoch BIGINT, seq_to BIGINT, files INTEGER)`.

`applied_envelopes` has three jobs. It makes checkpoints idempotent. It records which WAL sequences are already committed (checked at replay). It defines the per-node watermark that hides committed hot rows. See [Catalog and maintenance](catalog-and-maintenance.md) and [Query engine](query-engine.md).

## Deterministic identities

`crates/engine/src/ids.rs` holds the two identities that must be identical on every node, in every region and after every restart. Receivers compute both. Tenant and region always come from the receiving node's config (`node.tenant`, `node.region`), never from the payload (E2).

### Series id (spec section 4.4)

```text
series_id = xxh64(seed 0,  u32_le(len(tenant)) ‖ tenant
                         ‖ u32_le(len(region)) ‖ region
                         ‖ u32_le(len(metric)) ‖ metric
                         ‖ u32_le(len(canon))  ‖ canon)
canon     = for (k, v) in labels sorted by key: u32_le(len(k)) ‖ k ‖ u32_le(len(v)) ‖ v
```

Length prefixes make the encoding injective, so no label value can imitate a separator. The test `series_id_is_injective_on_separators` checks that `{a="b,c=d"}` and `{a="b", c="d"}` hash differently. `Labels` is a `BTreeMap`, so iteration order is already the canonical order. The spec's `series_id_collisions_total` counter (marked U) is not implemented.

### Log pattern hash (spec section 5.4)

`pattern_hash(body) = xxh64(seed 1, mask(body))`. `mask` applies five regexes in a fixed order, each replacing matches with `<*>`: UUIDs, IPv4 with optional port, hex runs of 8 or more characters, double-quoted strings, then numbers. Order matters. Numbers go last so they do not break up the UUIDs, addresses and hex runs first. `PATTERN_VERSION = 1` is stored in `logs.pattern_version` next to every hash. Changing a mask means bumping the version, because old and new hashes do not group together. Learned clustering (Drain-style) is deliberately not used for the hash, because it differs per node. The test `masking_groups_templates` and conformance case L4 (5 planted templates found exactly) cover it.

## DuckDB build and extensions

`duck::open` creates an in-memory database and loads three extensions: `icu`, `ducklake` and `httpfs`. `postgres_scanner` is loaded only when `catalog.url` starts with `postgres:`.

| Extension | Why |
| --- | --- |
| `ducklake` | The catalog (D3) |
| `httpfs` | S3 reads, plus the `CREATE SECRET tiny_eye_s3` that `duck::add_s3_secret` creates from `storage.s3` |
| `icu` | DuckLake needs `TIMESTAMPTZ` arithmetic, which DuckDB provides through ICU (code comment in `duck.rs`). It is loaded first, before `ducklake` |
| `postgres_scanner` | A PostgreSQL-backed DuckLake catalog (`postgres:<libpq conninfo>`) |

Extensions are shipped and pinned to the bundled DuckDB version (D12). The research machine could not reach the public extension host. `make tools` downloads the four extensions for the pinned DuckDB version (v2.0.1) into `.tools/duckdb_ext/<version>/<platform>/`. When `duckdb.extension_dir` is set, `duck::open` sets `extension_directory`, turns off `autoinstall_known_extensions` and `autoload_known_extensions`, and only runs `LOAD`. A missing extension then fails start with "run `make tools`". When `extension_dir` is empty, DuckDB installs extensions from its default location.

`duck::open` also sets `enable_external_file_cache` and `parquet_metadata_cache`, so repeated reads of the same files skip footer and data GETs ([Performance](performance.md) measures with and without these caches).

:::info DuckDB core is built without debuginfo
`Cargo.toml` sets `debug = false` for `libduckdb-sys` in both the dev and release profiles, plus `opt-level = 3` in dev. With debuginfo, the build defines `DEBUG`. DuckDB's DEBUG build counts allocations and aborts when the release-built `ducklake` and `httpfs` extensions free memory it did not see allocated. The PoC found this. Do not remove these profile overrides: a debug test run would abort inside DuckDB.
:::

## What the tests prove

`crates/engine/tests/poc.rs` keeps each storage assumption as a test that runs on local disk with a DuckDB-file catalog:

| Test | Proves |
| --- | --- |
| P1 `p1_writer_files_register_in_ducklake` | Files from the generated schema and our writer register through `ducklake_add_data_files`, with MAP, BLOB, TIMESTAMP_NS and nullable UBIGINT. A file with the wrong schema is refused (E3) |
| P6 `p6_bloom_on_trace_id` | 20,000 sorted spans at 8,192 rows per group give 3 row groups, each with a `trace_id` bloom filter |
| P7 `p7_arrow_appender_round_trip` | The Arrow appender accepts every generated column type into an in-memory table. DuckDB's sorted export is accepted by our writer and registers in DuckLake |
| `store::tests::hour_round_trips_through_key` | Key hour encoding and decoding agree |
| `ids::tests` | Series id injectivity, mask grouping |

P2 to P5 (hot tail, compaction swap, retention, TQS on lake views) are described in [Catalog and maintenance](catalog-and-maintenance.md) and [Query engine](query-engine.md).
