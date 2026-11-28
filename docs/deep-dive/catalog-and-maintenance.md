---
title: Catalog and maintenance
description: How DuckLake is used as the catalog, covering idempotent checkpoints through applied_envelopes, hot-row release, hourly compaction with an atomic swap, and retention that lets DuckLake delete objects.
sidebar_position: 4
---

The coordinator half of `crates/engine/src/node.rs` (`checkpoint`, `release_hot`, `compact`, `retain`) keeps the catalog in step with object storage. In the spec this is a separate leased role (section 6). In tiny-eye it runs in the same process as ingest and query, driven by `run_background`.

## DuckLake as the catalog

D3: DuckLake is the catalog. tiny-eye writes the Parquet files itself and registers them. From then on, DuckLake owns their lifecycle: snapshots, statistics-based pruning, and deletion.

- `duck::attach_lake` runs `ATTACH IF NOT EXISTS 'ducklake:<catalog.url>' AS lake (DATA_PATH '<storage url>/<region>/<id>/lake/')`.
- `catalog.url` is either a file path, which gives a DuckDB-file catalog (the single-node default, `./data/catalog.ducklake`), or `postgres:<libpq conninfo>`, which loads `postgres_scanner` and keeps the metadata in PostgreSQL. The research ran DuckLake on PostgreSQL 16 (E3). The tests and benchmarks here use the file catalog. The effective config (`tiny-eye config`) redacts `password=` and URL credentials in the conninfo.
- Files are registered with `CALL ducklake_add_data_files('lake', '<table>', '<absolute url>')`. They live outside `DATA_PATH` under our own keys, with absolute paths (D9). Each node has its own catalog, which names only that node's files; nodes sharing a bucket read each other's catalogs through published copies ([Run a fleet](../examples/run-a-fleet.md)). DuckLake records per-file min/max statistics, which is what prunes files during queries.

### Single coordinator, serialized writes

Every catalog write takes `catalog_lock`, an async mutex: checkpoint, compaction, retention and publishing the fleet catalog copy never overlap. The spec instead uses a lease object (conditional PUT with `If-Match`) plus a `coordinator_fence` row in every transaction (section 6.2, D7). Those exist to stop two processes from both believing they coordinate. With exactly one process per catalog that cannot happen, so the mutex gives the same guarantee. In a fleet every node still writes only its own catalog. The research left one race untested: a compaction swap concurrent with an ingest add on the same table. The lock rules that race out by construction.

## Checkpoint

`Node::checkpoint()` runs every `coordinator.checkpoint_interval` (5 s) and in `flush_and_checkpoint()`:

1. **List.** `commits/` under the node's own prefix `<region>/<id>/` is listed and sorted by key. With the zero-padded `<epoch>-<seq>` keys, that is commit order. Each object is fetched and parsed, and any `format` other than 1 fails the checkpoint with both versions named.
2. **Nothing pending.** If the list is empty, it calls `release_hot()` and returns 0.
3. **Apply, in one transaction.**

   ```sql
   BEGIN;
     -- for each envelope, in key order:
     SELECT count(*) FROM lake.applied_envelopes WHERE envelope_id = ?;   -- > 0: skip, already applied
     CALL ducklake_add_data_files('lake', '<table>', '<path>');          -- once per file
     INSERT INTO lake.applied_envelopes VALUES (?, ?, ?, ?, ?, ?);
   COMMIT;   -- or ROLLBACK on any error
   ```

   Table names are checked against `Table::from_name` before use. Any error rolls back the whole transaction, covering every envelope of the tick.
4. **Mark committed.** For this node's own envelopes in the current epoch, every `sealed` sequence up to `seq_to` gets its commit time (`now`) if it has none.
5. **Delete the envelope objects**, all of them, including those skipped as already applied.
6. **Release hot rows** (below), and count `tiny_eye_checkpoints_total`.

### Why it is idempotent

A crash between `COMMIT` and the envelope deletes leaves applied envelopes in storage. The next checkpoint finds each `envelope_id` in `applied_envelopes`, skips it, and deletes the object, so no file is registered twice (E3). The `envelope_id` is stable across flush retries, because `write_sequence` reads back the existing envelope when its create-only PUT finds one ([Write path](write-path.md)).

Registering a file twice must never happen anyway. DuckLake deletes registered files when they expire (E6). Two registrations of one object would mean the first expiry deletes data the second still references.

### Batching and its failure mode

The spec marks "batching all pending envelopes into one transaction per tick" as U. tiny-eye builds it, so each tick creates one catalog snapshot however many flushes happened. The benchmark measured 50 ms for a checkpoint on local disk and 93 ms on MinIO ([Performance](performance.md)).

:::warning A poison envelope blocks all checkpoints
Because one failure rolls back the whole batch, an envelope that can never apply stalls every later envelope. Examples: a file that was deleted, a schema mismatch after a `tables.yaml` change, an unknown table. The node keeps serving: views still show hot rows above the old watermark. But `hot_bytes` stops falling and ingest eventually answers 429. The error is logged every `checkpoint_interval` ("checkpoint failed; will retry"). The fix is operational: repair or move the offending object under `<region>/<id>/commits/`.
:::

## Releasing hot rows

`release_hot()` frees RAM once the lake has the rows:

1. Walk `sealed` in sequence order. Take the longest prefix whose commit time is at least `coordinator.hot_grace` (30 s) ago. `upto` is the last sequence of that prefix.
2. `DELETE FROM memory.main.hot_<t> WHERE _seq <= upto` for every table, on the writer connection.
3. Drop those sequences from `sealed` and subtract their bytes from `hot_bytes`.

Only a prefix is released, so a sequence is never deleted while an older one is still uncommitted. Visibility does not depend on the release. The read views already hide every hot row at or below the watermark the moment the commit is visible ([Query engine](query-engine.md)). The grace period covers readers whose lake snapshot predates the commit. Such a query sees the old watermark and so expects the rows in the hot table. Deleting them immediately could let it see neither copy. 30 s is a generous margin over the gap between a query's first catalog read and its hot-table scan. The tests set `hot_grace: 1ms` so they can observe the release.

## Compaction

`Node::compact(now)` runs every `coordinator.compaction_interval` (1 min). It merges each closed hour's files into one L1 file per table (spec section 6.3, D4).

**Selection.** For each table, `ducklake_list_files('lake', '<table>')` lists the registered files. Each file's hour is parsed from its key (`date=`/`hour=`), and files whose URL is not under this store's prefix are ignored. The files are grouped by hour. A group is compacted when:

- it contains at least one `l0-` file, and
- the hour is closed plus the delay: `(hour + 1) * 3600000 + compaction_delay <= now` (default delay 10 min).

The group includes any existing L1 file of that hour. Late data (allowed up to `max_sample_age`, 1 h) therefore creates new L0 files in an already-compacted hour, and the next pass merges L0 plus the old L1 into a new single L1. `metrics_series` lives in each day's `hour=00` partition ([Storage format](storage-format.md)), so it is re-compacted after every flush that adds series rows for that day.

**Rewrite.** For each group:

```sql
SELECT <columns> FROM (SELECT * FROM read_parquet([<urls>], union_by_name = true) <dedup_sql>) ORDER BY <sort columns>
```

The rows are read straight from the file URLs, then deduplicated by the table's `unique` key across all of the hour's files and sorted. They are written with our Parquet writer, with the same encodings and bloom filters as L0, and PUT to `<table>/tenant=/date=/hour=/l1-<ulid>-compacted.parquet`.

For `metrics_points` the query also computes `positive_delta`, the reset-corrected increase since the previous sample of the same series (D2):

```sql
CASE WHEN lag(value) OVER w IS NULL THEN NULL
     WHEN value >= lag(value) OVER w THEN value - lag(value) OVER w
     ELSE value END                          -- WINDOW w AS (PARTITION BY series_id ORDER BY timestamp_ms)
```

D2 moved counter deltas out of ingest. The v5 ingest-state rule lost 9.33 % of increase under one crash, while computing at read or compaction time was exact (E5). No query reads `positive_delta` yet: the TQS compiler always recomputes from raw values. The window is also not seeded with the previous hour's last sample, so the first sample of each series in each hour has `NULL` (the spec marks that seeding as U). `positive_delta` makes L1 slightly larger than L0 in the benchmark.

**Swap.** With the new file in storage, one transaction replaces the old files:

```sql
BEGIN;
  DELETE FROM lake.<table> WHERE filename IN (<old urls>);
  CALL ducklake_add_data_files('lake', '<table>', '<new url>');
COMMIT;
```

The probe in the research (RESEARCH section 6) showed this is an atomic swap. A whole-file delete writes no delete file. E6 showed the row count is unchanged across an 11-to-2 file swap. Readers on an older snapshot keep reading the old files until the snapshot expires.

Notes for maintainers:

- All groups of one pass are rewritten into memory first, in one blocking task, before any PUT. After a long backlog (for example the first pass after a large backfill), the pass holds every new L1 file in RAM at once.
- A crash between the L1 PUT and the swap leaves an unreferenced L1 object, and the L0 files stay registered. The next pass compacts the hour again. Orphans need a lifecycle rule.
- The compactor's `read_parquet` bypasses DuckLake, which is safe only because `catalog_lock` stops a checkpoint from adding a file to the hour between the listing and the swap. A future multi-process coordinator must keep that property.
- Compaction is what makes trace lookups cheap. Each span file costs a footer GET plus a bloom GET when the id is absent (E4). The benchmark's 11 hourly L0 span files dropped from 22 GETs to 2 for an absent id after compaction ([Performance](performance.md)).

## Retention

`Node::retain(now)` runs once on the first background tick after start, then every `coordinator.retention_interval` (1 h) (spec section 6.4):

1. `cutoff_hour = (now - coordinator.retention) / 3600000` (default retention 15 days). For each table, every registered file whose key hour is below the cutoff is removed with `DELETE FROM lake.<table> WHERE filename IN (...)`.
2. `DELETE FROM lake.applied_envelopes WHERE envelope_id < '<ULID of now - 1 day>'`. ULIDs sort by time, so this keeps one day of envelope ids for the crash-replay checks of the checkpoint and the WAL replay.
3. **Expire superseded snapshots.** A snapshot is expired once a *newer* snapshot has existed for `coordinator.snapshot_grace` (default 10 min): the snapshot ids whose successor's `snapshot_time` is older than `now - snapshot_grace` go to `CALL ducklake_expire_snapshots('lake', versions => [...])`. The newest snapshot is never expired.
4. `CALL ducklake_cleanup_old_files('lake', cleanup_all => true)`. DuckLake deletes the objects that no live snapshot references. That covers both expired data and the L0 files swapped out by compaction.

### The snapshot grace

The rule used to be "expire snapshots older than 10 minutes". That is unsafe for a snapshot that stays current for a long time, for example on a quiet node: it is old, but it was the live state until a moment ago, and a query (or another node's catalog copy) may still name its files. Counting from when a snapshot was *replaced* fixes that.

The grace must cover the oldest reader:

- a query on this node, at most `query.timeout` (30 s);
- a query on another node that reads this node's published copy. The check allows `fleet.publish_interval + 3 × fleet.refresh_interval` for a copy to be published, found by the other node (one refresh may take up to a refresh interval) and replaced there, and a query on it can run `query.timeout` more.

So the configuration is refused unless `snapshot_grace >= fleet.publish_interval + 3 × fleet.refresh_interval + query.timeout` (150 s with the defaults; the default grace is 10 min). The grace protects copies that other nodes read, so every node of a fleet should use the same grace, intervals and `query.timeout`. The publisher also writes its grace into the manifest (`snapshot_grace_ms`), and other nodes skip a manifest older than the smaller of that and their own grace, so they never attach a copy whose files may already be gone. That comparison uses the earlier of the publisher's timestamp and the bucket's last-modified time against the reader's clock, so keep the nodes' clocks synced with NTP. `retention_on_one_node_keeps_copies_readable_within_the_grace` in `crates/engine/tests/fleet.rs` compacts one node while another reads its copy: within the grace the replaced L0 files are kept and the answers are unchanged, past it they are deleted and the stale manifest is skipped. With the old rule the test fails.

DuckLake deletes files it did not write, including files outside its `DATA_PATH` (E6, research finding 3). Retention therefore needs no object listing of its own. The rules that follow from this: never register an object twice, give the process delete rights on the bucket, and treat S3 lifecycle rules only as a backstop (the spec suggests retention plus 7 days) and as the cleaner for orphaned uploads.

Retention works at whole-file, whole-hour granularity, judged by the key's hour. Data is therefore kept for at least `retention`, and for at most `retention` plus 1 h plus one `retention_interval`.

## Configuration

| Key | Default | Effect |
| --- | --- | --- |
| `catalog.url` | `./data/catalog.ducklake` | DuckDB file or `postgres:<conninfo>` |
| `coordinator.checkpoint_interval` | `5s` | How often pending envelopes are applied |
| `coordinator.hot_grace` | `30s` | Hot rows are kept this long after their commit |
| `coordinator.compaction_interval` | `1m` | How often closed hours are checked |
| `coordinator.compaction_delay` | `10m` | An hour is compacted this long after it closes |
| `coordinator.retention` | `15d` | Data older than this is dropped |
| `coordinator.retention_interval` | `1h` | How often retention runs |
| `coordinator.snapshot_grace` | `10m` | A snapshot is expired once a newer one has existed this long; at least `fleet.publish_interval + 3 × fleet.refresh_interval + query.timeout` |

## What the tests prove

| Test | Proves |
| --- | --- |
| PoC P2 `p2_hot_tail_counted_once` | A watermark view over an in-memory table counts 2,000 rows once before flush, after the commit, and after the hot rows are dropped. A naive union would give 4,000 (E7) |
| PoC P3 + P4 `p3_compaction_swap_and_p4_retention` | Five L0 files swapped for one L1 in one transaction: same row count, same `sum(value)`, one file, `positive_delta` filled. After snapshot expiry and cleanup, DuckLake deleted all five L0 files and kept the L1 (E6) |
| `ingest_flush_checkpoint_compact_retain` | Checkpoint applies one envelope. A second checkpoint after the grace releases every hot row. Compaction of two flushes in one hour keeps 63 points and deduplicates `metrics_series` to 3 rows. Retention in the far future empties the table |
| `failed_flush_is_retried_and_loses_nothing` | Hot rows are released only after their sequence is committed |
| `retention_on_one_node_keeps_copies_readable_within_the_grace` (`fleet.rs`) | Retention keeps the files a superseded snapshot names for `snapshot_grace`, so another node's catalog copy stays readable; past the grace they are deleted |
