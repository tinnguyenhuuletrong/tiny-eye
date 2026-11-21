# tiny-eye benchmark — data on local disk

Machine: 8 logical CPUs, macos. Engine: DuckDB v2.0.1, 4 DuckDB threads, durability `wal` (fsync per group). HTTP API in process, 4 concurrent remote-write clients.

Data: 2620 series (2000 counters, 500 gauges, 120 histogram buckets), 6 h at a 15 s scrape interval.

## Ingest (Prometheus remote-write over HTTP)

| Samples | Seconds | Samples/s | Request p50 ms | p99 ms |
| ---: | ---: | ---: | ---: | ---: |
| 3668000 | 2.6 | 1404145 | 14.8 | 22.6 |

## Write path

| Step | Result |
| :--- | :--- |
| Flush of the whole backfill (sort, Parquet, PUT, envelope) | 0.9 s |
| Checkpoint (apply envelope to DuckLake) | 60 ms |
| L0: points files / bytes / bytes per sample | 7 / 1463503 / 0.40 |
| Series table bytes | 46750 |
| Compaction (all hours, all tables) | 8 file groups in 1.6 s |
| L1: points files / bytes / bytes per sample | 7 / 1557326 / 0.42 |

## Queries (HTTP round trip, 10 runs after one warm-up)

### 6 h range, step 60 s, L0 files

| Panel | Points | p50 ms | p95 ms | max ms |
| :--- | ---: | ---: | ---: | ---: |
| `sum by (job) (rate(http_requests_total[5m]))` | 3410 | 3732.7 | 3850.4 | 3850.4 |
| `topk(10, sum by (route) (rate(…[5m])))` | 3410 | 3745.9 | 3902.3 | 3902.3 |
| `error ratio formula (2 queries)` | 1023 | 4253.4 | 4451.0 | 4451.0 |
| `histogram_quantile(0.99, … by (job, le))` | 3410 | 433.0 | 447.4 | 447.4 |
| `max by (job) (gauge)` | 3410 | 1087.4 | 1116.9 | 1116.9 |
| `one series: rate(…{instance="10.0.0.1:9100"})` | 341 | 130.1 | 136.5 | 136.5 |
| `table: sum by (code) (increase(…[$__range]))` | 2 | 401.5 | 404.9 | 404.9 |

### 6 h range, step 60 s, after compaction (L1)

| Panel | Points | p50 ms | p95 ms | max ms |
| :--- | ---: | ---: | ---: | ---: |
| `sum by (job) (rate(http_requests_total[5m]))` | 3410 | 3693.6 | 3832.3 | 3832.3 |
| `topk(10, sum by (route) (rate(…[5m])))` | 3410 | 3713.7 | 3750.6 | 3750.6 |
| `error ratio formula (2 queries)` | 1023 | 4256.3 | 4401.1 | 4401.1 |
| `histogram_quantile(0.99, … by (job, le))` | 3410 | 435.8 | 439.4 | 439.4 |
| `max by (job) (gauge)` | 3410 | 1101.1 | 1108.4 | 1108.4 |
| `one series: rate(…{instance="10.0.0.1:9100"})` | 341 | 131.8 | 140.0 | 140.0 |
| `table: sum by (code) (increase(…[$__range]))` | 2 | 428.2 | 439.9 | 439.9 |

### last 1 h, step 15 s, last 10 min only in the hot tail

| Panel | Points | p50 ms | p95 ms | max ms |
| :--- | ---: | ---: | ---: | ---: |
| `sum by (job) (rate(http_requests_total[5m]))` | 2410 | 1455.8 | 1463.3 | 1463.3 |
| `topk(10, sum by (route) (rate(…[5m])))` | 2410 | 1476.2 | 1481.2 | 1481.2 |
| `error ratio formula (2 queries)` | 723 | 1686.8 | 1711.0 | 1711.0 |
| `histogram_quantile(0.99, … by (job, le))` | 2410 | 259.6 | 263.1 | 263.1 |
| `max by (job) (gauge)` | 2410 | 383.2 | 407.7 | 407.7 |
| `one series: rate(…{instance="10.0.0.1:9100"})` | 241 | 120.7 | 122.5 | 122.5 |
| `table: sum by (code) (increase(…[$__range]))` | 2 | 155.1 | 162.8 | 162.8 |

## Traces (OTLP/HTTP protobuf)

119988 spans ingested and flushed 11 times in 1.5 s (81635 spans/s); 33.5 bytes per span on L0.

### Trace lookup by id, 11 L0 files (11 span files)

| Trace id | Spans | p50 ms | p95 ms | GETs, cold caches |
| :--- | ---: | ---: | ---: | ---: |
| present | 6 | 27.7 | 29.7 | — |
| absent | 0 | 26.3 | 27.1 | — |

### Trace lookup by id, after compaction (1 span files)

| Trace id | Spans | p50 ms | p95 ms | GETs, cold caches |
| :--- | ---: | ---: | ---: | ---: |
| present | 6 | 32.0 | 32.6 | — |
| absent | 0 | 25.3 | 27.5 | — |

Reproduce: `make bench` (local disk) or `make bench-s3` (MinIO). Numbers are from one run on one machine; MinIO runs on the same host, so S3 latency here is ~1 ms, not a real region's ~20–50 ms.
