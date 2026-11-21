# tiny-eye benchmark — data on S3 (MinIO, same host)

Machine: 8 logical CPUs, macos. Engine: DuckDB v2.0.1, 4 DuckDB threads, durability `wal` (fsync per group). HTTP API in process, 4 concurrent remote-write clients.

Data: 2620 series (2000 counters, 500 gauges, 120 histogram buckets), 6 h at a 15 s scrape interval.

## Ingest (Prometheus remote-write over HTTP)

| Samples | Seconds | Samples/s | Request p50 ms | p99 ms |
| ---: | ---: | ---: | ---: | ---: |
| 3668000 | 2.2 | 1653032 | 12.1 | 18.2 |

## Write path

| Step | Result |
| :--- | :--- |
| Flush of the whole backfill (sort, Parquet, PUT, envelope) | 0.6 s |
| Checkpoint (apply envelope to DuckLake) | 93 ms |
| L0: points files / bytes / bytes per sample | 6 / 1444764 / 0.39 |
| Series table bytes | 46750 |
| Compaction (all hours, all tables) | 7 file groups in 1.1 s |
| L1: points files / bytes / bytes per sample | 6 / 1533317 / 0.42 |

## Queries (HTTP round trip, 10 runs after one warm-up)

### 6 h range, step 60 s, L0 files

| Panel | Points | p50 ms | p95 ms | max ms |
| :--- | ---: | ---: | ---: | ---: |
| `sum by (job) (rate(http_requests_total[5m]))` | 3410 | 582.4 | 594.6 | 594.6 |
| `topk(10, sum by (route) (rate(…[5m])))` | 3410 | 586.2 | 591.0 | 591.0 |
| `error ratio formula (2 queries)` | 1023 | 586.6 | 590.3 | 590.3 |
| `histogram_quantile(0.99, … by (job, le))` | 3410 | 69.8 | 70.8 | 70.8 |
| `max by (job) (gauge)` | 3410 | 100.0 | 100.6 | 100.6 |
| `one series: rate(…{instance="10.0.0.1:9100"})` | 341 | 20.8 | 21.8 | 21.8 |
| `table: sum by (code) (increase(…[$__range]))` | 2 | 393.4 | 397.6 | 397.6 |

### 6 h range, step 60 s, after compaction (L1)

| Panel | Points | p50 ms | p95 ms | max ms |
| :--- | ---: | ---: | ---: | ---: |
| `sum by (job) (rate(http_requests_total[5m]))` | 3410 | 583.4 | 585.2 | 585.2 |
| `topk(10, sum by (route) (rate(…[5m])))` | 3410 | 589.9 | 596.5 | 596.5 |
| `error ratio formula (2 queries)` | 1023 | 596.9 | 606.1 | 606.1 |
| `histogram_quantile(0.99, … by (job, le))` | 3410 | 73.5 | 74.7 | 74.7 |
| `max by (job) (gauge)` | 3410 | 100.5 | 102.4 | 102.4 |
| `one series: rate(…{instance="10.0.0.1:9100"})` | 341 | 20.6 | 21.3 | 21.3 |
| `table: sum by (code) (increase(…[$__range]))` | 2 | 396.9 | 414.8 | 414.8 |

### last 1 h, step 15 s, last 10 min only in the hot tail

| Panel | Points | p50 ms | p95 ms | max ms |
| :--- | ---: | ---: | ---: | ---: |
| `sum by (job) (rate(http_requests_total[5m]))` | 2410 | 223.0 | 235.4 | 235.4 |
| `topk(10, sum by (route) (rate(…[5m])))` | 2410 | 218.3 | 230.6 | 230.6 |
| `error ratio formula (2 queries)` | 723 | 205.3 | 207.4 | 207.4 |
| `histogram_quantile(0.99, … by (job, le))` | 2410 | 39.9 | 41.1 | 41.1 |
| `max by (job) (gauge)` | 2410 | 49.5 | 51.4 | 51.4 |
| `one series: rate(…{instance="10.0.0.1:9100"})` | 241 | 19.6 | 20.0 | 20.0 |
| `table: sum by (code) (increase(…[$__range]))` | 2 | 83.3 | 84.6 | 84.6 |

## Traces (OTLP/HTTP protobuf)

119988 spans ingested and flushed 11 times in 1.3 s (94925 spans/s); 33.5 bytes per span on L0.

### Trace lookup by id, 11 L0 files (11 span files)

| Trace id | Spans | p50 ms | p95 ms | GETs, cold caches |
| :--- | ---: | ---: | ---: | ---: |
| present | 6 | 11.0 | 11.1 | 24 |
| absent | 0 | 10.2 | 10.7 | 22 |

### Trace lookup by id, after compaction (1 span files)

| Trace id | Spans | p50 ms | p95 ms | GETs, cold caches |
| :--- | ---: | ---: | ---: | ---: |
| present | 6 | 12.8 | 13.7 | 15 |
| absent | 0 | 9.1 | 9.4 | 2 |

**Caveats.** Synthetic counters rise linearly, so bytes per sample (0.4) is far below real data (the research measured ~6 B/sample on random walks; measure on real exporter data before sizing). L1 is slightly larger than L0 because compaction adds `positive_delta`. Panel timings are with warm DuckDB caches (`enable_external_file_cache`); the GET column is measured with caches off.

Reproduce: `make bench` (local disk) or `make bench-s3` (MinIO). Numbers are from one run on one machine; MinIO runs on the same host, so S3 latency here is ~1 ms, not a real region's ~20–50 ms.
