---
title: Performance
description: What tiny-eye's benchmark measures, the numbers it produced on one machine, what dominates each cost, the window-evaluation change behind the rate panels, and the caveats that limit what the numbers mean.
sidebar_position: 6
---

All numbers on this page come from `tiny-eye-bench` (`crates/server/src/bin/bench.rs`), written to [bench-local](evidence/bench-local.md) by `make bench` and to [bench-s3](evidence/bench-s3.md) by `make bench-s3`. They come from one run on one 8-CPU macOS laptop, with DuckDB v2.0.1. Read the caveats before using any number for sizing.

## What the benchmark does

The benchmark runs a real `Node` in process behind its HTTP API, which is the same router and handlers as the binary. It runs on local disk or on MinIO (same host). The DuckDB settings are 4 threads, an 8 GB memory limit and 4 query workers. Durability is `wal`, so every request waits for a group fsync. The config makes the phases explicit: `flush_interval: 1h` and `checkpoint_interval: 1h` keep the background loop out of the way, the benchmark calls `flush_now`, `checkpoint` and `compact` itself, `arena_limit: 4GB` absorbs the backfill, and `max_sample_age: 30d` admits it.

Phases:

1. **Remote-write backfill.** 2,620 series: 2,000 counters `http_requests_total{job, instance, route, code}`, 500 gauges, and 120 histogram buckets (10 jobs × 12 `le`). 6 hours at a 15 s scrape, except the last 10 minutes. 4 concurrent clients send requests of at most 2,000 series × 4 steps, and retry on 429.
2. **Flush and checkpoint** of the whole backfill, then bytes per sample on L0.
3. **Seven dashboard panels** over 6 h at a 60 s step on L0 files, then compaction, then the same panels on L1. Each panel is warmed once, then run 10 times over HTTP.
4. **Live data.** The last 10 minutes are written and left in the hot tail. The panels then run over the last hour at a 15 s step.
5. **Traces.** 20,000 traces of 6 spans over 3 services within one hour, sent as OTLP/HTTP protobuf in 11 flushes, so there are 11 L0 span files. Lookup by id (present and absent) runs before and after compaction. On S3 the benchmark also counts GET requests from DuckDB's HTTP log with the external file cache and metadata cache turned off.

## Results

### Ingest and write path

| Measure | Local disk | MinIO |
| --- | --- | --- |
| Remote-write: 3,668,000 samples | 2.2 s, **1.65 M samples/s** | 2.2 s, 1.65 M samples/s |
| Request latency p50 / p99 | 12.1 / 18.4 ms | 12.1 / 18.2 ms |
| Flush of the whole backfill (sort, Parquet, PUT, envelope) | 0.5 s | 0.6 s |
| Checkpoint (apply the envelope) | 50 ms | 93 ms |
| L0 `metrics_points`: files / bytes / bytes per sample | 6 / 1,448,309 / 0.39 | 6 / 1,444,764 / 0.39 |
| `metrics_series` bytes | 46,750 | 46,750 |
| Compaction, all hours and tables | 7 file groups, 1.0 s | 7 file groups, 1.1 s |
| L1 `metrics_points`: files / bytes / bytes per sample | 6 / 1,536,923 / 0.42 | 6 / 1,533,317 / 0.42 |
| Spans: 119,988 in 11 flushes | 1.0 s (117k spans/s), 33.5 B/span | 1.3 s (95k spans/s), 33.5 B/span |

Ingest throughput with fsync on comes from group commit. Concurrent requests queue on the WAL thread, which writes them together and syncs once ([Write path](write-path.md)). The 7 compaction groups are consistent with 6 hourly `metrics_points` groups plus the one daily `hour=00` partition of `metrics_series` ([Storage format](storage-format.md)).

### Panels (p50, HTTP round trip)

| Panel | Points | 6 h, L0 | 6 h, L1 | Last 1 h, 15 s step, hot tail |
| --- | ---: | ---: | ---: | ---: |
| `sum by (job) (rate(http_requests_total[5m]))` | 3,410 | 582 ms | 584 ms | 223 ms |
| `topk(10, sum by (route) (rate(...[5m])))` | 3,410 | 582 ms | 587 ms | 220 ms |
| Error ratio formula (2 queries) | 1,023 | 603 ms | 596 ms | 205 ms |
| `histogram_quantile(0.99, ... by (job, le))` | 3,410 | 73 ms | 73 ms | 42 ms |
| `max by (job) (gauge)` | 3,410 | 103 ms | 102 ms | 45 ms |
| One series: `rate(...{instance="10.0.0.1:9100"})` | 341 | 21 ms | 21 ms | 20 ms |
| Table: `sum by (code) (increase(...[$__range]))` | 2 | 392 ms | 397 ms | 83 ms |

The local-disk figures are shown. MinIO is within a few percent on every panel, because the caches are warm and MinIO runs on the same host.

### Trace lookup by id

| State | Spans | p50 local | p50 MinIO | GETs, cold caches (MinIO) |
| --- | ---: | ---: | ---: | ---: |
| Present id, 11 L0 files | 6 | 11.2 ms | 11.0 ms | 24 |
| Absent id, 11 L0 files | 0 | 11.1 ms | 10.2 ms | **22** |
| Present id, after compaction (1 file) | 6 | 13.1 ms | 12.8 ms | 15 |
| Absent id, after compaction (1 file) | 0 | 9.7 ms | 9.1 ms | **2** |

## What dominates each cost

**Rate-family panels (about 0.58 s).** The panel covers 2,000 counter series over 341 steps, with about 1,380 samples per series in range (including the 5-minute lookback before the first step). Each query runs a per-series window over every sample in the range (`lag`, then the running sums of the fast path). It then makes two ASOF probes per (series, step), about 682,000 of each, before aggregating into 10 or 20 keys. The cost scales with samples plus series × steps, not with step count alone. `topk` and the two-query formula cost the same because the work is in the per-series stage.

**Histogram and gauge panels (73 ms, 103 ms).** The same pipeline runs over far fewer series: 120 bucket series, and 500 gauges with `last`.

**One series (21 ms).** A single series' points. This is close to the fixed cost of any query here: HTTP, validation, the catalog snapshot read, and scanning `metrics_series` to select the series.

**Table over `$__range` (about 0.39 s).** One step, but a 6-hour window. Every sample in the range still enters `pts` and the running window, so the cost is O(samples) even though only one point per key is returned.

**L0 vs L1.** No difference for metrics. The backfill was one flush, so each hour already had exactly one points file. Compaction only rewrote the files, adding `positive_delta`. For spans, where 11 flushes produced 11 files in one hour, compaction matters a lot.

**Hot tail.** The 1-hour panels read L1 files plus the hot table through the same views. They are faster than the 6-hour panels because they cover less data, even at a finer step.

**Trace lookups.** Random trace ids defeat min/max pruning, so for an absent id every span file costs a footer GET plus a bloom-filter GET. 22 GETs for 11 files is exactly 2 per file, the same number research E4 measured (22 GETs for 11 files). After compaction the hour has one file, so the lookup needs 2 GETs. A present id also reads data pages: 24 GETs before compaction, 15 after. This is why compaction is hourly and why the spec wants a time range on every lookup (section 7.2): the GET count, not local latency, is what grows with file count on real S3. The latency columns barely move here because the caches are warm and MinIO is local. The present-id lookup is slightly slower after compaction (13.1 vs 11.2 ms), most likely because it decodes a larger row group from the one file. That cause has not been profiled.

## The window-evaluation change: 3.7 s to 0.58 s

The 6-hour `sum by (job) (rate(...[5m]))` panel took about 3.7 s while every range function was evaluated by the join path. It takes 0.58 s with the fast path.

The **join path** (`grid JOIN pts ON ts in (t - w, t]`, then `GROUP BY t, series_id`) materializes every (step, series, sample-in-window) triple. With a 5-minute window at a 15 s scrape, each window holds 20 samples, so this panel builds about 2,000 × 341 × 20 ≈ 13.6 million rows before grouping.

The **fast path** (`per_cte` in `crates/tqs/src/compile.rs`) computes running sums once per series: row number, reset correction, reset count, change count. It then finds each window's first and last sample with two ASOF joins against the (step, series) grid. Every statistic the rate family needs is a difference or an endpoint of those: `n = i_last - i_first + 1`, `corr = c_corr_last - c_corr_first`, the first and last values and times. The work drops to one pass over the samples plus about 682,000 probes per join, O(series × steps) instead of O(series × steps × samples per window). The saving grows with the window. A `rate(...[1h])` panel over the same data would build 240 samples per window on the join path.

The join path is kept for `avg_over_time`, `min_over_time`, `max_over_time` and `sum_over_time`. Min and max are not differences of prefix sums. Sum and average would be the difference of two large running sums, and that loses precision through cancellation, enough to break the 1e-9 equality with Prometheus that the conformance suite enforces. Both paths are covered by the same conformance cases: M01 to M04, M18, M24 and M25 exercise the fast path, and M12 to M15 the join path.

To find where a heavy panel spends its time, `crates/server/tests/profile.rs` prints DuckDB's `EXPLAIN ANALYZE` for a 2,000-series, 6-hour rate panel:

```bash
cargo test --release -p server --test profile -- --ignored --nocapture
```

## Caveats

- **Synthetic data compresses too well.** The benchmark's counters rise linearly, which gives 0.39 bytes per sample on L0. The research measured 6.18 B/sample on L0 and 4.60 on L1 with random walks (E5). Real exporter data will land in between. Measure on it before sizing storage.
- **One run, one machine.** There are no repetitions across runs and no variance figures beyond p50/p95 within a run.
- **MinIO on the same host** has about 1 ms latency, not a real region's 20 to 50 ms per request. Only the GET counts carry over to real S3. Latencies on S3 will be higher, especially for cold trace lookups and the first read of each file.
- **Warm caches.** Panel timings are taken with `enable_external_file_cache` and `parquet_metadata_cache` on, after a warm-up run. Only the GET column is measured cold.
- **One big flush.** The backfill is flushed once, so L0 is already one file per hour. With the default `flush_interval: 60s` a real deployment produces up to 60 L0 files per hour per node before compaction, and queries over the open and the last closed hour read all of them.
- **Not measured yet.** Queries on a PostgreSQL catalog, concurrent query load, `memory` durability throughput, and memory use of the hot tail at the arena limit. The spec's v1.0 trace-lookup target on real S3 is still to be set after a first real benchmark (spec section 1.5).

## Knobs that change these numbers

| Key | Effect |
| --- | --- |
| `duckdb.threads` | Parallelism inside one statement (shared by all connections) |
| `duckdb.memory_limit` | DuckDB's memory cap, which covers the hot tables and query working memory |
| `query.workers` | Concurrent queries |
| `query.timeout` | Upper bound on one statement (the interrupt fires after it) |
| `ingest.flush_bytes`, `ingest.flush_interval` | L0 file count and size, and how much data the hot tail holds |
| `ingest.row_group_rows` | Row-group size: pruning granularity versus per-group overhead |
| `coordinator.compaction_delay`, `compaction_interval` | How long an hour stays as many L0 files |
| `ingest.durability` | `memory` skips the WAL fsync |

## Reproduce

```bash
make bench       # local disk  -> docs/deep-dive/evidence/bench-local.md
make bench-s3    # MinIO       -> docs/deep-dive/evidence/bench-s3.md
```

The binary also takes `--series`, `--hours`, `--traces`, `--s3 <endpoint>` and `--out <file>` (header of `bench.rs`). See [Testing and evidence](testing-and-evidence.md) for the other evidence suites.
