# tiny-eye benchmark — data on S3 (MinIO, same host)

> **Run:** 2026-11-28 12:26 UTC · commit `e62b87c686` · GitHub Actions, `ubuntu24` 20261122.23.1 runner ([run 46142991662](https://github.com/tinnguyenhuuletrong/tiny-eye/actions/runs/46142991662)) · 4 logical CPUs (Intel(R) Xeon(R) 6975P-C), 15 GB RAM, linux

Engine: DuckDB v2.0.1, 4 DuckDB threads, durability `wal` (fsync per group). HTTP API in process, 4 concurrent remote-write clients.

Data: 2620 series (2000 counters, 500 gauges, 120 histogram buckets), 6 h at a 15 s scrape interval.

## Ingest (Prometheus remote-write over HTTP)

| Samples | Seconds | Samples/s | Request p50 ms | p99 ms |
| ---: | ---: | ---: | ---: | ---: |
| 3668000 | 2.7 | 1358281 | 14.4 | 25.7 |

## Write path

| Step | Result |
| :--- | :--- |
| Flush of the whole backfill (sort, Parquet, PUT, envelope) | 1.1 s |
| Checkpoint (apply envelope to DuckLake) | 92 ms |
| L0: points files / bytes / bytes per sample | 7 / 1475120 / 0.40 |
| Series table bytes | 46750 |
| Compaction (all hours, all tables) | 8 file groups in 2.0 s |
| L1: points files / bytes / bytes per sample | 7 / 1568217 / 0.43 |

## Queries (HTTP round trip, 10 runs after one warm-up)

### 6 h range, step 60 s, L0 files

| Panel | Points | p50 ms | p95 ms | max ms |
| :--- | ---: | ---: | ---: | ---: |
| `sum by (job) (rate(http_requests_total[5m]))` | 3410 | 961.0 | 1026.1 | 1026.1 |
| `topk(10, sum by (route) (rate(…[5m])))` | 3410 | 973.7 | 1036.4 | 1036.4 |
| `error ratio formula (2 queries)` | 1023 | 1002.3 | 1075.5 | 1075.5 |
| `histogram_quantile(0.99, … by (job, le))` | 3410 | 113.6 | 167.0 | 167.0 |
| `max by (job) (gauge)` | 3410 | 184.0 | 227.7 | 227.7 |
| `one series: rate(…{instance="10.0.0.1:9100"})` | 341 | 30.8 | 32.1 | 32.1 |
| `table: sum by (code) (increase(…[$__range]))` | 2 | 651.4 | 697.3 | 697.3 |

### 6 h range, step 60 s, after compaction (L1)

| Panel | Points | p50 ms | p95 ms | max ms |
| :--- | ---: | ---: | ---: | ---: |
| `sum by (job) (rate(http_requests_total[5m]))` | 3410 | 984.3 | 1046.8 | 1046.8 |
| `topk(10, sum by (route) (rate(…[5m])))` | 3410 | 985.8 | 1056.6 | 1056.6 |
| `error ratio formula (2 queries)` | 1023 | 1015.6 | 1090.9 | 1090.9 |
| `histogram_quantile(0.99, … by (job, le))` | 3410 | 114.5 | 180.5 | 180.5 |
| `max by (job) (gauge)` | 3410 | 186.9 | 214.4 | 214.4 |
| `one series: rate(…{instance="10.0.0.1:9100"})` | 341 | 32.4 | 33.7 | 33.7 |
| `table: sum by (code) (increase(…[$__range]))` | 2 | 663.2 | 693.8 | 693.8 |

### last 1 h, step 15 s, last 10 min only in the hot tail

| Panel | Points | p50 ms | p95 ms | max ms |
| :--- | ---: | ---: | ---: | ---: |
| `sum by (job) (rate(http_requests_total[5m]))` | 2410 | 393.7 | 404.8 | 404.8 |
| `topk(10, sum by (route) (rate(…[5m])))` | 2410 | 395.7 | 412.9 | 412.9 |
| `error ratio formula (2 queries)` | 723 | 362.7 | 390.8 | 390.8 |
| `histogram_quantile(0.99, … by (job, le))` | 2410 | 68.5 | 90.3 | 90.3 |
| `max by (job) (gauge)` | 2410 | 87.6 | 113.1 | 113.1 |
| `one series: rate(…{instance="10.0.0.1:9100"})` | 241 | 31.4 | 32.8 | 32.8 |
| `table: sum by (code) (increase(…[$__range]))` | 2 | 141.9 | 163.5 | 163.5 |

## Materialized panels

The 6 timeseries panels above with `materialize: true` (resolution 15 s, settle 5 min). Reads use the UI's default step for a 6 h range (120 s), in process, 10 runs after one warm-up each. "Equal" compares every point with the raw query on the same grid (relative 1e-9).

| Step | Result |
| :--- | :--- |
| Catch-up over 6 h (once; the background loop never backfills) | 62490 points in 6.4 s |
| One background tick (one new 15 s step for every panel) | 44 points in 278 ms |

| Panel | Points | Raw p50 ms | Materialized p50 / p95 ms | Faster (p50) | Stored / computed steps | Equal |
| :--- | ---: | ---: | ---: | ---: | ---: | :--- |
| `sum by (job) (rate(http_requests_total[5m]))` | 1800 | 805.1 | 65.1 / 111.5 | 12.4× | 1770 / 3 | yes |
| `topk(10, sum by (route) (rate(…[5m])))` | 1800 | 793.1 | 64.5 / 66.0 | 12.3× | 1770 / 3 | yes |
| `error ratio formula (2 queries)` | 540 | 906.2 | 85.8 / 136.6 | 10.6× | 531 / 3 | yes |
| `histogram_quantile(0.99, … by (job, le))` | 1800 | 104.3 | 47.4 / 66.5 | 2.2× | 1770 / 3 | yes |
| `max by (job) (gauge)` | 1810 | 170.5 | 41.2 / 42.1 | 4.1× | 1780 / 3 | yes |
| `one series: rate(…{instance="10.0.0.1:9100"})` | 180 | 32.7 | 32.5 / 34.2 | 1.0× | 177 / 3 | yes |

### Read cost against range (`sum by (job) (rate(…[5m]))`, the UI's default step for each range)

| Range | Step | Points | Raw p50 ms | Materialized p50 ms | Faster |
| :--- | ---: | ---: | ---: | ---: | ---: |
| 1 h | 15 s | 2410 | 395.6 | 81.4 | 4.9× |
| 3 h | 60 s | 1810 | 486.4 | 92.5 | 5.3× |
| 6 h | 120 s | 1800 | 795.7 | 66.1 | 12.0× |

### Coverage after uptime (6 h read of `sum by (job) (rate(…))`)

| Panel stored for | Raw p50 ms | Materialized p50 ms | Faster |
| :--- | ---: | ---: | ---: |
| 0 h (just added) | 797.3 | 848.5 | 0.9× |
| 1 h | 801.9 | 808.4 | 1.0× |
| 6 h | 799.6 | 66.0 | 12.1× |

### Ad-hoc queries (6 h, relative time, no step: what an agent or `curl` sends)

| Query | p50 ms | Store used |
| :--- | ---: | :--- |
| same query as the panel, other `id` and `title` | 92.5 | yes |
| different query (`window: 4m`) | 819.7 | no (computed raw) |

### Background cost and storage

| Measure | Result |
| :--- | :--- |
| One background tick, 8 panels (p50 of 10) | 390 ms every 15 s (2.6 % of one connection's time) |
| Per panel per tick | 49 ms |
| Stored points / file size | 65394 / 1.6 MB (24.2 bytes per point) |

### Memory and disk bounds

The background task keeps one slice in flight and writes it straight to the store, so nothing accumulates in the process. Bounds (config `materialize.*`): at most 64 panels, 1000000 stored points per panel (oldest steps dropped past it), 200000 points per slice.

| Measure | Result |
| :--- | :--- |
| Process RSS before / after 240 more ticks (1 h of steps) | 420 MB / 310 MB (peak 444 MB, change -110 MB) |
| Tick p50 / max over those ticks | 249 / 424 ms |
| Stored points before / after | 65394 / 67188 |
| Worst case the config allows | 64 panels x 1000000 points x ~24 B = 1536 MB of store |

## Traces (OTLP/HTTP protobuf)

119988 spans ingested and flushed 11 times in 1.4 s (88486 spans/s); 33.5 bytes per span on L0.

### Trace lookup by id, 11 L0 files (11 span files)

| Trace id | Spans | p50 ms | p95 ms | GETs, cold caches |
| :--- | ---: | ---: | ---: | ---: |
| present | 6 | 18.4 | 18.6 | 24 |
| absent | 0 | 15.9 | 16.2 | 22 |

### Trace lookup by id, after compaction (1 span files)

| Trace id | Spans | p50 ms | p95 ms | GETs, cold caches |
| :--- | ---: | ---: | ---: | ---: |
| present | 6 | 21.4 | 23.0 | 15 |
| absent | 0 | 14.5 | 15.1 | 2 |

**Caveats.** Synthetic counters rise linearly, so bytes per sample (0.4) is far below real data (the research measured ~6 B/sample on random walks; measure on real exporter data before sizing). L1 is slightly larger than L0 because compaction adds `positive_delta`. Panel timings are with warm DuckDB caches (`enable_external_file_cache`); the GET column is measured with caches off.

Reproduce: `make bench` (local disk) or `make bench-s3` (MinIO). Numbers are from one run on one machine; MinIO runs on the same host, so S3 latency here is ~1 ms, not a real region's ~20–50 ms.
