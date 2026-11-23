# tiny-eye benchmark — data on local disk

Machine: 8 logical CPUs, macos. Engine: DuckDB v2.0.1, 4 DuckDB threads, durability `wal` (fsync per group). HTTP API in process, 4 concurrent remote-write clients.

Data: 2620 series (2000 counters, 500 gauges, 120 histogram buckets), 6 h at a 15 s scrape interval.

## Ingest (Prometheus remote-write over HTTP)

| Samples | Seconds | Samples/s | Request p50 ms | p99 ms |
| ---: | ---: | ---: | ---: | ---: |
| 3668000 | 2.4 | 1537912 | 12.8 | 22.6 |

## Write path

| Step | Result |
| :--- | :--- |
| Flush of the whole backfill (sort, Parquet, PUT, envelope) | 0.9 s |
| Checkpoint (apply envelope to DuckLake) | 87 ms |
| L0: points files / bytes / bytes per sample | 7 / 1476942 / 0.40 |
| Series table bytes | 46750 |
| Compaction (all hours, all tables) | 8 file groups in 1.1 s |
| L1: points files / bytes / bytes per sample | 7 / 1569227 / 0.43 |

## Queries (HTTP round trip, 10 runs after one warm-up)

### 6 h range, step 60 s, L0 files

| Panel | Points | p50 ms | p95 ms | max ms |
| :--- | ---: | ---: | ---: | ---: |
| `sum by (job) (rate(http_requests_total[5m]))` | 3410 | 655.0 | 754.6 | 754.6 |
| `topk(10, sum by (route) (rate(…[5m])))` | 3410 | 605.6 | 617.2 | 617.2 |
| `error ratio formula (2 queries)` | 1023 | 592.2 | 624.6 | 624.6 |
| `histogram_quantile(0.99, … by (job, le))` | 3410 | 69.9 | 71.2 | 71.2 |
| `max by (job) (gauge)` | 3410 | 100.7 | 103.1 | 103.1 |
| `one series: rate(…{instance="10.0.0.1:9100"})` | 341 | 20.8 | 21.3 | 21.3 |
| `table: sum by (code) (increase(…[$__range]))` | 2 | 386.7 | 391.0 | 391.0 |

### 6 h range, step 60 s, after compaction (L1)

| Panel | Points | p50 ms | p95 ms | max ms |
| :--- | ---: | ---: | ---: | ---: |
| `sum by (job) (rate(http_requests_total[5m]))` | 3410 | 611.7 | 619.5 | 619.5 |
| `topk(10, sum by (route) (rate(…[5m])))` | 3410 | 583.3 | 604.5 | 604.5 |
| `error ratio formula (2 queries)` | 1023 | 594.0 | 611.9 | 611.9 |
| `histogram_quantile(0.99, … by (job, le))` | 3410 | 71.3 | 72.4 | 72.4 |
| `max by (job) (gauge)` | 3410 | 100.7 | 102.0 | 102.0 |
| `one series: rate(…{instance="10.0.0.1:9100"})` | 341 | 21.6 | 22.8 | 22.8 |
| `table: sum by (code) (increase(…[$__range]))` | 2 | 391.2 | 398.8 | 398.8 |

### last 1 h, step 15 s, last 10 min only in the hot tail

| Panel | Points | p50 ms | p95 ms | max ms |
| :--- | ---: | ---: | ---: | ---: |
| `sum by (job) (rate(http_requests_total[5m]))` | 2410 | 216.8 | 218.6 | 218.6 |
| `topk(10, sum by (route) (rate(…[5m])))` | 2410 | 217.7 | 219.5 | 219.5 |
| `error ratio formula (2 queries)` | 723 | 203.1 | 205.4 | 205.4 |
| `histogram_quantile(0.99, … by (job, le))` | 2410 | 41.7 | 42.2 | 42.2 |
| `max by (job) (gauge)` | 2410 | 51.6 | 58.9 | 58.9 |
| `one series: rate(…{instance="10.0.0.1:9100"})` | 241 | 20.6 | 21.3 | 21.3 |
| `table: sum by (code) (increase(…[$__range]))` | 2 | 84.0 | 85.8 | 85.8 |

## Materialized panels

The 6 timeseries panels above with `materialize: true` (resolution 15 s, settle 5 min). Reads use the UI's default step for a 6 h range (120 s), in process, 10 runs after one warm-up each. "Equal" compares every point with the raw query on the same grid (relative 1e-9).

| Step | Result |
| :--- | :--- |
| Catch-up over 6 h (once; the background loop never backfills) | 62490 points in 3.7 s |
| One background tick (one new 15 s step for every panel) | 44 points in 167 ms |

| Panel | Points | Raw p50 ms | Materialized p50 / p95 ms | Faster (p50) | Stored / computed steps | Equal |
| :--- | ---: | ---: | ---: | ---: | ---: | :--- |
| `sum by (job) (rate(http_requests_total[5m]))` | 1800 | 442.7 | 37.2 / 39.8 | 11.9× | 1770 / 3 | yes |
| `topk(10, sum by (route) (rate(…[5m])))` | 1800 | 443.0 | 37.5 / 39.0 | 11.8× | 1770 / 3 | yes |
| `error ratio formula (2 queries)` | 540 | 499.1 | 47.2 / 48.1 | 10.6× | 531 / 3 | yes |
| `histogram_quantile(0.99, … by (job, le))` | 1800 | 62.1 | 26.9 / 27.7 | 2.3× | 1770 / 3 | yes |
| `max by (job) (gauge)` | 1810 | 91.1 | 23.1 / 25.1 | 3.9× | 1780 / 3 | yes |
| `one series: rate(…{instance="10.0.0.1:9100"})` | 180 | 21.4 | 20.3 / 22.5 | 1.1× | 177 / 3 | yes |

### Read cost against range (`sum by (job) (rate(…[5m]))`, the UI's default step for each range)

| Range | Step | Points | Raw p50 ms | Materialized p50 ms | Faster |
| :--- | ---: | ---: | ---: | ---: | ---: |
| 1 h | 15 s | 2410 | 228.6 | 44.7 | 5.1× |
| 3 h | 60 s | 1810 | 269.8 | 39.5 | 6.8× |
| 6 h | 120 s | 1800 | 440.9 | 36.7 | 12.0× |

### Coverage after uptime (6 h read of `sum by (job) (rate(…))`)

| Panel stored for | Raw p50 ms | Materialized p50 ms | Faster |
| :--- | ---: | ---: | ---: |
| 0 h (just added) | 438.0 | 443.7 | 1.0× |
| 1 h | 441.0 | 431.2 | 1.0× |
| 6 h | 449.6 | 37.2 | 12.1× |

### Ad-hoc queries (6 h, relative time, no step: what an agent or `curl` sends)

| Query | p50 ms | Store used |
| :--- | ---: | :--- |
| same query as the panel, other `id` and `title` | 58.4 | yes |
| different query (`window: 4m`) | 441.4 | no (computed raw) |

### Background cost and storage

| Measure | Result |
| :--- | :--- |
| One background tick, 8 panels (p50 of 10) | 244 ms every 15 s (1.6 % of one connection's time) |
| Per panel per tick | 30 ms |
| Stored points / file size | 65394 / 1.6 MB (24.2 bytes per point) |

### Memory and disk bounds

The background task keeps one slice in flight and writes it straight to the store, so nothing accumulates in the process. Bounds (config `materialize.*`): at most 64 panels, 1000000 stored points per panel (oldest steps dropped past it), 200000 points per slice.

| Measure | Result |
| :--- | :--- |
| Process RSS before / after 240 more ticks (1 h of steps) | 1737 MB / 1589 MB (peak 1749 MB, change -148 MB) |
| Tick p50 / max over those ticks | 152 / 245 ms |
| Stored points before / after | 65394 / 67188 |
| Worst case the config allows | 64 panels x 1000000 points x ~24 B = 1536 MB of store |

## Traces (OTLP/HTTP protobuf)

119988 spans ingested and flushed 11 times in 1.1 s (113924 spans/s); 33.5 bytes per span on L0.

### Trace lookup by id, 11 L0 files (11 span files)

| Trace id | Spans | p50 ms | p95 ms | GETs, cold caches |
| :--- | ---: | ---: | ---: | ---: |
| present | 6 | 11.1 | 15.2 | — |
| absent | 0 | 10.1 | 10.9 | — |

### Trace lookup by id, after compaction (1 span files)

| Trace id | Spans | p50 ms | p95 ms | GETs, cold caches |
| :--- | ---: | ---: | ---: | ---: |
| present | 6 | 12.7 | 13.2 | — |
| absent | 0 | 9.1 | 9.4 | — |

**Caveats.** Synthetic counters rise linearly, so bytes per sample (0.4) is far below real data (the research measured ~6 B/sample on random walks; measure on real exporter data before sizing). L1 is slightly larger than L0 because compaction adds `positive_delta`. Panel timings are with warm DuckDB caches (`enable_external_file_cache`); the GET column is measured with caches off.

Reproduce: `make bench` (local disk) or `make bench-s3` (MinIO). Numbers are from one run on one machine; MinIO runs on the same host, so S3 latency here is ~1 ms, not a real region's ~20–50 ms.
