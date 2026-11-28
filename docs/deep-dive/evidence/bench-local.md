# tiny-eye benchmark — data on local disk

> **Run:** 2026-11-28 12:21 UTC · commit `e62b87c686` · GitHub Actions, `ubuntu24` 20261122.23.1 runner ([run 46142991662](https://github.com/tinnguyenhuuletrong/tiny-eye/actions/runs/46142991662)) · 4 logical CPUs (Intel(R) Xeon(R) 6975P-C), 15 GB RAM, linux

Engine: DuckDB v2.0.1, 4 DuckDB threads, durability `wal` (fsync per group). HTTP API in process, 4 concurrent remote-write clients.

Data: 2620 series (2000 counters, 500 gauges, 120 histogram buckets), 6 h at a 15 s scrape interval.

## Ingest (Prometheus remote-write over HTTP)

| Samples | Seconds | Samples/s | Request p50 ms | p99 ms |
| ---: | ---: | ---: | ---: | ---: |
| 3668000 | 2.6 | 1402072 | 14.4 | 23.4 |

## Write path

| Step | Result |
| :--- | :--- |
| Flush of the whole backfill (sort, Parquet, PUT, envelope) | 1.0 s |
| Checkpoint (apply envelope to DuckLake) | 64 ms |
| L0: points files / bytes / bytes per sample | 7 / 1464320 / 0.40 |
| Series table bytes | 46750 |
| Compaction (all hours, all tables) | 8 file groups in 2.0 s |
| L1: points files / bytes / bytes per sample | 7 / 1557390 / 0.42 |

## Queries (HTTP round trip, 10 runs after one warm-up)

### 6 h range, step 60 s, L0 files

| Panel | Points | p50 ms | p95 ms | max ms |
| :--- | ---: | ---: | ---: | ---: |
| `sum by (job) (rate(http_requests_total[5m]))` | 3410 | 957.2 | 1039.7 | 1039.7 |
| `topk(10, sum by (route) (rate(…[5m])))` | 3410 | 976.5 | 1033.6 | 1033.6 |
| `error ratio formula (2 queries)` | 1023 | 1004.0 | 1077.9 | 1077.9 |
| `histogram_quantile(0.99, … by (job, le))` | 3410 | 114.4 | 167.6 | 167.6 |
| `max by (job) (gauge)` | 3410 | 175.9 | 218.8 | 218.8 |
| `one series: rate(…{instance="10.0.0.1:9100"})` | 341 | 31.8 | 33.1 | 33.1 |
| `table: sum by (code) (increase(…[$__range]))` | 2 | 660.4 | 671.2 | 671.2 |

### 6 h range, step 60 s, after compaction (L1)

| Panel | Points | p50 ms | p95 ms | max ms |
| :--- | ---: | ---: | ---: | ---: |
| `sum by (job) (rate(http_requests_total[5m]))` | 3410 | 990.3 | 1051.9 | 1051.9 |
| `topk(10, sum by (route) (rate(…[5m])))` | 3410 | 996.6 | 1062.4 | 1062.4 |
| `error ratio formula (2 queries)` | 1023 | 1016.7 | 1097.5 | 1097.5 |
| `histogram_quantile(0.99, … by (job, le))` | 3410 | 119.8 | 178.0 | 178.0 |
| `max by (job) (gauge)` | 3410 | 180.6 | 224.6 | 224.6 |
| `one series: rate(…{instance="10.0.0.1:9100"})` | 341 | 33.2 | 34.9 | 34.9 |
| `table: sum by (code) (increase(…[$__range]))` | 2 | 690.4 | 714.7 | 714.7 |

### last 1 h, step 15 s, last 10 min only in the hot tail

| Panel | Points | p50 ms | p95 ms | max ms |
| :--- | ---: | ---: | ---: | ---: |
| `sum by (job) (rate(http_requests_total[5m]))` | 2410 | 398.3 | 415.8 | 415.8 |
| `topk(10, sum by (route) (rate(…[5m])))` | 2410 | 399.4 | 423.2 | 423.2 |
| `error ratio formula (2 queries)` | 723 | 371.4 | 400.9 | 400.9 |
| `histogram_quantile(0.99, … by (job, le))` | 2410 | 70.1 | 102.2 | 102.2 |
| `max by (job) (gauge)` | 2410 | 81.5 | 87.5 | 87.5 |
| `one series: rate(…{instance="10.0.0.1:9100"})` | 241 | 31.9 | 32.7 | 32.7 |
| `table: sum by (code) (increase(…[$__range]))` | 2 | 147.3 | 165.0 | 165.0 |

## Materialized panels

The 6 timeseries panels above with `materialize: true` (resolution 15 s, settle 5 min). Reads use the UI's default step for a 6 h range (120 s), in process, 10 runs after one warm-up each. "Equal" compares every point with the raw query on the same grid (relative 1e-9).

| Step | Result |
| :--- | :--- |
| Catch-up over 6 h (once; the background loop never backfills) | 62490 points in 6.5 s |
| One background tick (one new 15 s step for every panel) | 44 points in 280 ms |

| Panel | Points | Raw p50 ms | Materialized p50 / p95 ms | Faster (p50) | Stored / computed steps | Equal |
| :--- | ---: | ---: | ---: | ---: | ---: | :--- |
| `sum by (job) (rate(http_requests_total[5m]))` | 1810 | 798.3 | 101.7 / 103.3 | 7.9× | 1780 / 3 | yes |
| `topk(10, sum by (route) (rate(…[5m])))` | 1810 | 803.0 | 102.1 / 151.5 | 7.9× | 1780 / 3 | yes |
| `error ratio formula (2 queries)` | 543 | 921.2 | 142.2 / 190.3 | 6.5× | 534 / 3 | yes |
| `histogram_quantile(0.99, … by (job, le))` | 1810 | 103.2 | 79.7 / 142.0 | 1.3× | 1780 / 3 | yes |
| `max by (job) (gauge)` | 1810 | 162.2 | 68.3 / 69.2 | 2.4× | 1780 / 3 | yes |
| `one series: rate(…{instance="10.0.0.1:9100"})` | 181 | 34.5 | 58.5 / 62.4 | 0.6× | 178 / 3 | yes |

### Read cost against range (`sum by (job) (rate(…[5m]))`, the UI's default step for each range)

| Range | Step | Points | Raw p50 ms | Materialized p50 ms | Faster |
| :--- | ---: | ---: | ---: | ---: | ---: |
| 1 h | 15 s | 2410 | 399.3 | 96.3 | 4.1× |
| 3 h | 60 s | 1810 | 494.3 | 97.5 | 5.1× |
| 6 h | 120 s | 1810 | 829.4 | 102.5 | 8.1× |

### Coverage after uptime (6 h read of `sum by (job) (rate(…))`)

| Panel stored for | Raw p50 ms | Materialized p50 ms | Faster |
| :--- | ---: | ---: | ---: |
| 0 h (just added) | 812.6 | 836.8 | 1.0× |
| 1 h | 800.8 | 823.2 | 1.0× |
| 6 h | 810.5 | 101.7 | 8.0× |

### Ad-hoc queries (6 h, relative time, no step: what an agent or `curl` sends)

| Query | p50 ms | Store used |
| :--- | ---: | :--- |
| same query as the panel, other `id` and `title` | 90.3 | yes |
| different query (`window: 4m`) | 825.0 | no (computed raw) |

### Background cost and storage

| Measure | Result |
| :--- | :--- |
| One background tick, 8 panels (p50 of 10) | 394 ms every 15 s (2.6 % of one connection's time) |
| Per panel per tick | 49 ms |
| Stored points / file size | 65394 / 1.6 MB (24.2 bytes per point) |

### Memory and disk bounds

The background task keeps one slice in flight and writes it straight to the store, so nothing accumulates in the process. Bounds (config `materialize.*`): at most 64 panels, 1000000 stored points per panel (oldest steps dropped past it), 200000 points per slice.

| Measure | Result |
| :--- | :--- |
| Process RSS before / after 240 more ticks (1 h of steps) | 665 MB / 520 MB (peak 665 MB, change -145 MB) |
| Tick p50 / max over those ticks | 255 / 425 ms |
| Stored points before / after | 65394 / 67188 |
| Worst case the config allows | 64 panels x 1000000 points x ~24 B = 1536 MB of store |

## Traces (OTLP/HTTP protobuf)

119988 spans ingested and flushed 11 times in 1.4 s (88270 spans/s); 33.5 bytes per span on L0.

### Trace lookup by id, 11 L0 files (11 span files)

| Trace id | Spans | p50 ms | p95 ms | GETs, cold caches |
| :--- | ---: | ---: | ---: | ---: |
| present | 6 | 18.1 | 19.7 | — |
| absent | 0 | 16.6 | 17.2 | — |

### Trace lookup by id, after compaction (1 span files)

| Trace id | Spans | p50 ms | p95 ms | GETs, cold caches |
| :--- | ---: | ---: | ---: | ---: |
| present | 6 | 20.7 | 23.2 | — |
| absent | 0 | 14.6 | 15.3 | — |

**Caveats.** Synthetic counters rise linearly, so bytes per sample (0.4) is far below real data (the research measured ~6 B/sample on random walks; measure on real exporter data before sizing). L1 is slightly larger than L0 because compaction adds `positive_delta`. Panel timings are with warm DuckDB caches (`enable_external_file_cache`); the GET column is measured with caches off.

Reproduce: `make bench` (local disk) or `make bench-s3` (MinIO). Numbers are from one run on one machine; MinIO runs on the same host, so S3 latency here is ~1 ms, not a real region's ~20–50 ms.
