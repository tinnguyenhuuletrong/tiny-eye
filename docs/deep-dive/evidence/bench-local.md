# tiny-eye benchmark — data on local disk

Machine: 8 logical CPUs, macos. Engine: DuckDB v2.0.1, 4 DuckDB threads, durability `wal` (fsync per group). HTTP API in process, 4 concurrent remote-write clients.

Data: 2620 series (2000 counters, 500 gauges, 120 histogram buckets), 6 h at a 15 s scrape interval.

## Ingest (Prometheus remote-write over HTTP)

| Samples | Seconds | Samples/s | Request p50 ms | p99 ms |
| ---: | ---: | ---: | ---: | ---: |
| 3668000 | 2.3 | 1617970 | 12.2 | 18.2 |

## Write path

| Step | Result |
| :--- | :--- |
| Flush of the whole backfill (sort, Parquet, PUT, envelope) | 0.7 s |
| Checkpoint (apply envelope to DuckLake) | 50 ms |
| L0: points files / bytes / bytes per sample | 7 / 1471089 / 0.40 |
| Series table bytes | 46750 |
| Compaction (all hours, all tables) | 8 file groups in 1.1 s |
| L1: points files / bytes / bytes per sample | 7 / 1565583 / 0.43 |

## Queries (HTTP round trip, 10 runs after one warm-up)

### 6 h range, step 60 s, L0 files

| Panel | Points | p50 ms | p95 ms | max ms |
| :--- | ---: | ---: | ---: | ---: |
| `sum by (job) (rate(http_requests_total[5m]))` | 3410 | 577.4 | 585.0 | 585.0 |
| `topk(10, sum by (route) (rate(…[5m])))` | 3410 | 576.5 | 584.9 | 584.9 |
| `error ratio formula (2 queries)` | 1023 | 585.5 | 594.1 | 594.1 |
| `histogram_quantile(0.99, … by (job, le))` | 3410 | 70.4 | 71.7 | 71.7 |
| `max by (job) (gauge)` | 3410 | 92.2 | 93.7 | 93.7 |
| `one series: rate(…{instance="10.0.0.1:9100"})` | 341 | 20.6 | 21.1 | 21.1 |
| `table: sum by (code) (increase(…[$__range]))` | 2 | 389.5 | 391.9 | 391.9 |

### 6 h range, step 60 s, after compaction (L1)

| Panel | Points | p50 ms | p95 ms | max ms |
| :--- | ---: | ---: | ---: | ---: |
| `sum by (job) (rate(http_requests_total[5m]))` | 3410 | 581.9 | 587.6 | 587.6 |
| `topk(10, sum by (route) (rate(…[5m])))` | 3410 | 612.6 | 636.0 | 636.0 |
| `error ratio formula (2 queries)` | 1023 | 591.5 | 593.8 | 593.8 |
| `histogram_quantile(0.99, … by (job, le))` | 3410 | 72.5 | 74.4 | 74.4 |
| `max by (job) (gauge)` | 3410 | 93.4 | 95.0 | 95.0 |
| `one series: rate(…{instance="10.0.0.1:9100"})` | 341 | 21.8 | 23.6 | 23.6 |
| `table: sum by (code) (increase(…[$__range]))` | 2 | 397.1 | 402.2 | 402.2 |

### last 1 h, step 15 s, last 10 min only in the hot tail

| Panel | Points | p50 ms | p95 ms | max ms |
| :--- | ---: | ---: | ---: | ---: |
| `sum by (job) (rate(http_requests_total[5m]))` | 2410 | 217.0 | 220.3 | 220.3 |
| `topk(10, sum by (route) (rate(…[5m])))` | 2410 | 218.2 | 221.7 | 221.7 |
| `error ratio formula (2 queries)` | 723 | 201.7 | 203.7 | 203.7 |
| `histogram_quantile(0.99, … by (job, le))` | 2410 | 41.4 | 41.7 | 41.7 |
| `max by (job) (gauge)` | 2410 | 45.2 | 46.7 | 46.7 |
| `one series: rate(…{instance="10.0.0.1:9100"})` | 241 | 20.4 | 21.1 | 21.1 |
| `table: sum by (code) (increase(…[$__range]))` | 2 | 85.8 | 88.1 | 88.1 |

## Materialized panels

The 6 timeseries panels above with `materialize: true` (resolution 15 s, settle 5 min). Reads use the UI's default step for a 6 h range (120 s), in process, 10 runs after one warm-up each. "Equal" compares every point with the raw query on the same grid (relative 1e-9).

| Step | Result |
| :--- | :--- |
| Catch-up over 6 h (once; the background loop never backfills) | 62490 points in 3.6 s |
| One background tick (one new 15 s step for every panel) | 44 points in 168 ms |

| Panel | Points | Raw p50 ms | Materialized p50 / p95 ms | Faster (p50) | Stored / computed steps | Equal |
| :--- | ---: | ---: | ---: | ---: | ---: | :--- |
| `sum by (job) (rate(http_requests_total[5m]))` | 1800 | 442.6 | 37.6 / 38.3 | 11.8× | 1770 / 3 | yes |
| `topk(10, sum by (route) (rate(…[5m])))` | 1800 | 445.9 | 38.5 / 41.7 | 11.6× | 1770 / 3 | yes |
| `error ratio formula (2 queries)` | 540 | 501.9 | 47.9 / 49.8 | 10.5× | 531 / 3 | yes |
| `histogram_quantile(0.99, … by (job, le))` | 1800 | 64.4 | 27.5 / 30.3 | 2.3× | 1770 / 3 | yes |
| `max by (job) (gauge)` | 1810 | 88.6 | 23.5 / 25.2 | 3.8× | 1780 / 3 | yes |
| `one series: rate(…{instance="10.0.0.1:9100"})` | 180 | 22.0 | 20.8 / 21.9 | 1.1× | 177 / 3 | yes |

### Read cost against range (`sum by (job) (rate(…[5m]))`, the UI's default step for each range)

| Range | Step | Points | Raw p50 ms | Materialized p50 ms | Faster |
| :--- | ---: | ---: | ---: | ---: | ---: |
| 1 h | 15 s | 2410 | 215.4 | 42.8 | 5.0× |
| 3 h | 60 s | 1810 | 264.1 | 38.2 | 6.9× |
| 6 h | 120 s | 1800 | 459.5 | 39.7 | 11.6× |

### Coverage after uptime (6 h read of `sum by (job) (rate(…))`)

| Panel stored for | Raw p50 ms | Materialized p50 ms | Faster |
| :--- | ---: | ---: | ---: |
| 0 h (just added) | 444.1 | 442.2 | 1.0× |
| 1 h | 444.5 | 429.9 | 1.0× |
| 6 h | 442.9 | 37.8 | 11.7× |

### Ad-hoc queries (6 h, relative time, no step: what an agent or `curl` sends)

| Query | p50 ms | Store used |
| :--- | ---: | :--- |
| same query as the panel, other `id` and `title` | 57.2 | yes |
| different query (`window: 4m`) | 444.6 | no (computed raw) |

### Background cost and storage

| Measure | Result |
| :--- | :--- |
| One background tick, 8 panels (p50 of 10) | 229 ms every 15 s (1.5 % of one connection's time) |
| Per panel per tick | 29 ms |
| Stored points / file size | 65394 / 1.6 MB (24.2 bytes per point) |

## Traces (OTLP/HTTP protobuf)

119988 spans ingested and flushed 11 times in 1.1 s (107673 spans/s); 33.5 bytes per span on L0.

### Trace lookup by id, 11 L0 files (11 span files)

| Trace id | Spans | p50 ms | p95 ms | GETs, cold caches |
| :--- | ---: | ---: | ---: | ---: |
| present | 6 | 11.3 | 11.9 | — |
| absent | 0 | 10.2 | 11.0 | — |

### Trace lookup by id, after compaction (1 span files)

| Trace id | Spans | p50 ms | p95 ms | GETs, cold caches |
| :--- | ---: | ---: | ---: | ---: |
| present | 6 | 13.3 | 14.3 | — |
| absent | 0 | 9.3 | 10.1 | — |

**Caveats.** Synthetic counters rise linearly, so bytes per sample (0.4) is far below real data (the research measured ~6 B/sample on random walks; measure on real exporter data before sizing). L1 is slightly larger than L0 because compaction adds `positive_delta`. Panel timings are with warm DuckDB caches (`enable_external_file_cache`); the GET column is measured with caches off.

Reproduce: `make bench` (local disk) or `make bench-s3` (MinIO). Numbers are from one run on one machine; MinIO runs on the same host, so S3 latency here is ~1 ms, not a real region's ~20–50 ms.
