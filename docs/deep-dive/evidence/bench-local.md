# tiny-eye benchmark — data on local disk

Machine: 8 logical CPUs, macos. Engine: DuckDB v2.0.1, 4 DuckDB threads, durability `wal` (fsync per group). HTTP API in process, 4 concurrent remote-write clients.

Data: 2620 series (2000 counters, 500 gauges, 120 histogram buckets), 6 h at a 15 s scrape interval.

## Ingest (Prometheus remote-write over HTTP)

| Samples | Seconds | Samples/s | Request p50 ms | p99 ms |
| ---: | ---: | ---: | ---: | ---: |
| 3668000 | 2.2 | 1646826 | 12.1 | 18.3 |

## Write path

| Step | Result |
| :--- | :--- |
| Flush of the whole backfill (sort, Parquet, PUT, envelope) | 0.7 s |
| Checkpoint (apply envelope to DuckLake) | 53 ms |
| L0: points files / bytes / bytes per sample | 7 / 1468602 / 0.40 |
| Series table bytes | 46750 |
| Compaction (all hours, all tables) | 8 file groups in 1.2 s |
| L1: points files / bytes / bytes per sample | 7 / 1562553 / 0.43 |

## Queries (HTTP round trip, 10 runs after one warm-up)

### 6 h range, step 60 s, L0 files

| Panel | Points | p50 ms | p95 ms | max ms |
| :--- | ---: | ---: | ---: | ---: |
| `sum by (job) (rate(http_requests_total[5m]))` | 3410 | 582.8 | 594.0 | 594.0 |
| `topk(10, sum by (route) (rate(…[5m])))` | 3410 | 580.7 | 583.0 | 583.0 |
| `error ratio formula (2 queries)` | 1023 | 612.2 | 621.7 | 621.7 |
| `histogram_quantile(0.99, … by (job, le))` | 3410 | 74.2 | 103.4 | 103.4 |
| `max by (job) (gauge)` | 3410 | 95.2 | 131.1 | 131.1 |
| `one series: rate(…{instance="10.0.0.1:9100"})` | 341 | 22.0 | 42.8 | 42.8 |
| `table: sum by (code) (increase(…[$__range]))` | 2 | 393.0 | 400.9 | 400.9 |

### 6 h range, step 60 s, after compaction (L1)

| Panel | Points | p50 ms | p95 ms | max ms |
| :--- | ---: | ---: | ---: | ---: |
| `sum by (job) (rate(http_requests_total[5m]))` | 3410 | 583.6 | 595.6 | 595.6 |
| `topk(10, sum by (route) (rate(…[5m])))` | 3410 | 588.9 | 592.1 | 592.1 |
| `error ratio formula (2 queries)` | 1023 | 595.4 | 598.4 | 598.4 |
| `histogram_quantile(0.99, … by (job, le))` | 3410 | 72.8 | 73.7 | 73.7 |
| `max by (job) (gauge)` | 3410 | 94.0 | 95.4 | 95.4 |
| `one series: rate(…{instance="10.0.0.1:9100"})` | 341 | 22.0 | 22.9 | 22.9 |
| `table: sum by (code) (increase(…[$__range]))` | 2 | 394.8 | 398.7 | 398.7 |

### last 1 h, step 15 s, last 10 min only in the hot tail

| Panel | Points | p50 ms | p95 ms | max ms |
| :--- | ---: | ---: | ---: | ---: |
| `sum by (job) (rate(http_requests_total[5m]))` | 2410 | 219.5 | 222.6 | 222.6 |
| `topk(10, sum by (route) (rate(…[5m])))` | 2410 | 220.3 | 223.0 | 223.0 |
| `error ratio formula (2 queries)` | 723 | 201.7 | 204.2 | 204.2 |
| `histogram_quantile(0.99, … by (job, le))` | 2410 | 42.1 | 43.3 | 43.3 |
| `max by (job) (gauge)` | 2410 | 45.9 | 46.9 | 46.9 |
| `one series: rate(…{instance="10.0.0.1:9100"})` | 241 | 20.7 | 21.8 | 21.8 |
| `table: sum by (code) (increase(…[$__range]))` | 2 | 84.0 | 85.6 | 85.6 |

## Materialized panels

The 6 timeseries panels above with `materialize: true` (resolution 15 s, settle 5 min). Reads use the UI's default step for a 6 h range (120 s), in process, 10 runs after one warm-up each. "Equal" compares every point with the raw query on the same grid (relative 1e-9).

| Step | Result |
| :--- | :--- |
| Catch-up over 6 h (once; the background loop never backfills) | 62490 points in 8.0 s |
| One background tick (one new 15 s step for every panel) | 44 points in 175 ms |

| Panel | Points | Raw p50 ms | Materialized p50 ms | Faster | Stored / computed steps | Equal |
| :--- | ---: | ---: | ---: | ---: | ---: | :--- |
| `sum by (job) (rate(http_requests_total[5m]))` | 1800 | 456.0 | 35.3 | 12.9× | 1780 / 2 | yes |
| `topk(10, sum by (route) (rate(…[5m])))` | 1800 | 447.0 | 34.0 | 13.2× | 1780 / 2 | yes |
| `error ratio formula (2 queries)` | 540 | 498.6 | 44.8 | 11.1× | 534 / 2 | yes |
| `histogram_quantile(0.99, … by (job, le))` | 1800 | 63.2 | 27.3 | 2.3× | 1780 / 2 | yes |
| `max by (job) (gauge)` | 1800 | 88.0 | 22.8 | 3.9× | 1780 / 2 | yes |
| `one series: rate(…{instance="10.0.0.1:9100"})` | 180 | 21.4 | 20.2 | 1.1× | 178 / 2 | yes |

## Traces (OTLP/HTTP protobuf)

119988 spans ingested and flushed 11 times in 1.1 s (107529 spans/s); 33.5 bytes per span on L0.

### Trace lookup by id, 11 L0 files (11 span files)

| Trace id | Spans | p50 ms | p95 ms | GETs, cold caches |
| :--- | ---: | ---: | ---: | ---: |
| present | 6 | 11.2 | 13.3 | — |
| absent | 0 | 10.2 | 10.7 | — |

### Trace lookup by id, after compaction (1 span files)

| Trace id | Spans | p50 ms | p95 ms | GETs, cold caches |
| :--- | ---: | ---: | ---: | ---: |
| present | 6 | 13.2 | 14.0 | — |
| absent | 0 | 9.5 | 10.4 | — |

**Caveats.** Synthetic counters rise linearly, so bytes per sample (0.4) is far below real data (the research measured ~6 B/sample on random walks; measure on real exporter data before sizing). L1 is slightly larger than L0 because compaction adds `positive_delta`. Panel timings are with warm DuckDB caches (`enable_external_file_cache`); the GET column is measured with caches off.

Reproduce: `make bench` (local disk) or `make bench-s3` (MinIO). Numbers are from one run on one machine; MinIO runs on the same host, so S3 latency here is ~1 ms, not a real region's ~20–50 ms.
