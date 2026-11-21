# tiny-eye benchmark — data on local disk

Machine: 8 logical CPUs, macos. Engine: DuckDB v2.0.1, 4 DuckDB threads, durability `wal` (fsync per group). HTTP API in process, 4 concurrent remote-write clients.

Data: 2620 series (2000 counters, 500 gauges, 120 histogram buckets), 6 h at a 15 s scrape interval.

## Ingest (Prometheus remote-write over HTTP)

| Samples | Seconds | Samples/s | Request p50 ms | p99 ms |
| ---: | ---: | ---: | ---: | ---: |
| 3668000 | 2.2 | 1653989 | 12.1 | 18.4 |

## Write path

| Step | Result |
| :--- | :--- |
| Flush of the whole backfill (sort, Parquet, PUT, envelope) | 0.5 s |
| Checkpoint (apply envelope to DuckLake) | 50 ms |
| L0: points files / bytes / bytes per sample | 6 / 1448309 / 0.39 |
| Series table bytes | 46750 |
| Compaction (all hours, all tables) | 7 file groups in 1.0 s |
| L1: points files / bytes / bytes per sample | 6 / 1536923 / 0.42 |

## Queries (HTTP round trip, 10 runs after one warm-up)

### 6 h range, step 60 s, L0 files

| Panel | Points | p50 ms | p95 ms | max ms |
| :--- | ---: | ---: | ---: | ---: |
| `sum by (job) (rate(http_requests_total[5m]))` | 3410 | 582.4 | 592.1 | 592.1 |
| `topk(10, sum by (route) (rate(…[5m])))` | 3410 | 582.3 | 605.7 | 605.7 |
| `error ratio formula (2 queries)` | 1023 | 602.9 | 621.1 | 621.1 |
| `histogram_quantile(0.99, … by (job, le))` | 3410 | 73.1 | 76.3 | 76.3 |
| `max by (job) (gauge)` | 3410 | 103.2 | 104.6 | 104.6 |
| `one series: rate(…{instance="10.0.0.1:9100"})` | 341 | 21.1 | 22.4 | 22.4 |
| `table: sum by (code) (increase(…[$__range]))` | 2 | 392.4 | 409.5 | 409.5 |

### 6 h range, step 60 s, after compaction (L1)

| Panel | Points | p50 ms | p95 ms | max ms |
| :--- | ---: | ---: | ---: | ---: |
| `sum by (job) (rate(http_requests_total[5m]))` | 3410 | 583.7 | 590.5 | 590.5 |
| `topk(10, sum by (route) (rate(…[5m])))` | 3410 | 586.7 | 598.7 | 598.7 |
| `error ratio formula (2 queries)` | 1023 | 596.4 | 601.8 | 601.8 |
| `histogram_quantile(0.99, … by (job, le))` | 3410 | 72.8 | 74.8 | 74.8 |
| `max by (job) (gauge)` | 3410 | 101.7 | 105.0 | 105.0 |
| `one series: rate(…{instance="10.0.0.1:9100"})` | 341 | 21.2 | 22.1 | 22.1 |
| `table: sum by (code) (increase(…[$__range]))` | 2 | 397.1 | 400.3 | 400.3 |

### last 1 h, step 15 s, last 10 min only in the hot tail

| Panel | Points | p50 ms | p95 ms | max ms |
| :--- | ---: | ---: | ---: | ---: |
| `sum by (job) (rate(http_requests_total[5m]))` | 2410 | 222.6 | 233.7 | 233.7 |
| `topk(10, sum by (route) (rate(…[5m])))` | 2410 | 219.5 | 223.3 | 223.3 |
| `error ratio formula (2 queries)` | 723 | 204.8 | 207.8 | 207.8 |
| `histogram_quantile(0.99, … by (job, le))` | 2410 | 42.0 | 43.5 | 43.5 |
| `max by (job) (gauge)` | 2410 | 45.4 | 46.4 | 46.4 |
| `one series: rate(…{instance="10.0.0.1:9100"})` | 241 | 20.3 | 21.2 | 21.2 |
| `table: sum by (code) (increase(…[$__range]))` | 2 | 83.3 | 85.3 | 85.3 |

## Traces (OTLP/HTTP protobuf)

119988 spans ingested and flushed 11 times in 1.0 s (117183 spans/s); 33.5 bytes per span on L0.

### Trace lookup by id, 11 L0 files (11 span files)

| Trace id | Spans | p50 ms | p95 ms | GETs, cold caches |
| :--- | ---: | ---: | ---: | ---: |
| present | 6 | 11.2 | 11.5 | — |
| absent | 0 | 11.1 | 11.7 | — |

### Trace lookup by id, after compaction (1 span files)

| Trace id | Spans | p50 ms | p95 ms | GETs, cold caches |
| :--- | ---: | ---: | ---: | ---: |
| present | 6 | 13.1 | 13.5 | — |
| absent | 0 | 9.7 | 14.4 | — |

**Caveats.** Synthetic counters rise linearly, so bytes per sample (0.4) is far below real data (the research measured ~6 B/sample on random walks; measure on real exporter data before sizing). L1 is slightly larger than L0 because compaction adds `positive_delta`. Panel timings are with warm DuckDB caches (`enable_external_file_cache`); the GET column is measured with caches off.

Reproduce: `make bench` (local disk) or `make bench-s3` (MinIO). Numbers are from one run on one machine; MinIO runs on the same host, so S3 latency here is ~1 ms, not a real region's ~20–50 ms.
