# Share-nothing fleet — proof of concept

Every node keeps its own DuckLake catalog and object prefix and publishes a copy of its catalog; a read-only viewer attaches all copies and queries them as one. Compared against a baseline node that holds every region's data in one catalog. Storage: MinIO on the same host (S3 API, ~1 ms per request). Machine: 8 logical CPUs, macos. DuckDB v2.0.1.

## Data

3 regions, each 745 series (500 counters, 125 gauges, 120 histogram buckets) over 6 h at 15 s, and 6000 traces × 6 spans: 3218400 samples and 108000 spans in total. Every region's series carry `cluster=<region>`. The baseline node received the same requests. All nodes flushed, checkpointed and compacted (one L1 file per table and hour).

## Publish (each node, after compaction)

| Node | Copy ms | Gzip ms | PUT ms (both) | Snapshot bytes | Gzipped bytes |
| :--- | ---: | ---: | ---: | ---: | ---: |
| region-0 | 15 | 1 | 49 | 5255168 | 43290 |
| region-1 | 15 | 1 | 46 | 5255168 | 43249 |
| region-2 | 15 | 1 | 44 | 5255168 | 43258 |

## Viewer: attach and refresh (3 snapshots)

The viewer reaches object storage through a TCP proxy that delays each direction by half the round trip (chunks stay pipelined, bandwidth is not limited): a laptop that far from the buckets.

| Round trip | Mode | First download ms | First attach + views ms | Refresh download ms | Refresh attach + swap ms |
| ---: | :--- | ---: | ---: | ---: | ---: |
| 0 ms | attached from object storage | — | 158 | — | 130 |
| 0 ms | downloaded, attached locally | 18 | 51 | 17 | 53 |
| 50 ms | attached from object storage | — | 1954 | — | 1979 |
| 50 ms | downloaded, attached locally | 193 | 59 | 192 | 58 |
| 150 ms | attached from object storage | — | 4995 | — | 5006 |
| 150 ms | downloaded, attached locally | 502 | 63 | 492 | 57 |

## Queries: viewer against the baseline node

Every query ran on the baseline node (one catalog with every region's data, next to the bucket) and on the viewer (UNION ALL over 3 attached catalogs). "First" runs with DuckDB's caches empty (counting the previous query's GETs switches them off, which empties them), so it is what a panel costs the first time; p50 is over 10 warm runs. Results equal to the baseline on every viewer configuration: **yes** (relative tolerance 1e-9; trace rows compared without the `region` column, which the viewer reports per source region and the baseline as `all`).

| Query | Rows | GETs, caches off: baseline / viewer | Baseline p50 ms | 0 ms, remote: first / p50 | 0 ms, downloaded: first / p50 | 50 ms, remote: first / p50 | 50 ms, downloaded: first / p50 | 150 ms, remote: first / p50 | 150 ms, downloaded: first / p50 |
| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `sum by (job) (rate(http_requests_total[5m]))` | 3510 | 47 / 87 | 477.8 | 627 / 482 | 541 / 480 | 1846 / 478 | 901 / 488 | 3979 / 479 | 1577 / 513 |
| `topk(10, sum by (route) (rate(…[5m])))` | 3510 | 47 / 87 | 480.7 | 502 / 483 | 513 / 483 | 1355 / 482 | 1457 / 531 | 2698 / 515 | 2814 / 484 |
| `error ratio formula (2 queries)` | 1053 | 94 / 174 | 501.8 | 516 / 490 | 516 / 486 | 1472 / 487 | 1453 / 491 | 3264 / 485 | 3141 / 487 |
| `histogram_quantile(0.99, … by (job, le))` | 3510 | 47 / 87 | 135.8 | 158 / 134 | 214 / 134 | 1089 / 136 | 1031 / 134 | 2633 / 134 | 2614 / 134 |
| `max by (job) (gauge)` | 3510 | 47 / 87 | 86.2 | 106 / 81 | 103 / 89 | 848 / 83 | 827 / 82 | 2529 / 82 | 2211 / 82 |
| `one series: rate(…{instance="10.0.0.1:9100"})` | 1053 | 41 / 102 | 22.7 | 50 / 30 | 70 / 33 | 729 / 30 | 725 / 31 | 2299 / 30 | 2147 / 30 |
| `table: sum by (code) (increase(…[$__range]))` | 2 | 47 / 87 | 319.7 | 356 / 332 | 371 / 371 | 1231 / 329 | 1246 / 332 | 2652 / 334 | 2981 / 403 |
| `trace by id, present` | 6 | 15 / 14 | 10.3 | 93 / 15 | 73 / 17 | 225 / 15 | 220 / 15 | 557 / 15 | 551 / 16 |
| `trace by id, absent` | 0 | 2 / 6 | 7.4 | 47 / 14 | 46 / 15 | 237 / 15 | 232 / 16 | 534 / 14 | 792 / 14 |

## Freshness: one new sample from region-0 to the viewer (no added round trip)

| Step | ms |
| :--- | ---: |
| Flush (Parquet + PUT + envelope) | 29 |
| Checkpoint (register in the node's catalog) | 32 |
| Publish: copy catalog + gzip | 18 |
| Publish: PUT | 71 |
| Viewer refresh: download 3 snapshots | 16 |
| Viewer refresh: attach + swap views | 116 |
| **Work on the path** | **281** |

Visible on the viewer before: no; after: yes. The lag a user sees is set by the intervals, not this work: at most `flush_interval` + `checkpoint_interval` + publish interval + viewer refresh interval.


## Catalog growth on one node (240 flush + checkpoint cycles, each one scrape of every series)

| After | Files in catalog | DuckLake snapshots | Snapshot bytes | Gzipped bytes | Copy + gzip ms | PUT ms | Viewer refresh ms |
| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 0 flushes | 11 | 17 | 5255168 | 43655 | 16 | 46 | 73 |
| 60 flushes | 131 | 77 | 5255168 | 52166 | 17 | 51 | 72 |
| 120 flushes | 251 | 137 | 5255168 | 59311 | 19 | 47 | 74 |
| 180 flushes | 371 | 197 | 5255168 | 65332 | 18 | 48 | 69 |
| 240 flushes | 491 | 257 | 5255168 | 73586 | 18 | 51 | 72 |
| then compaction + snapshot expiry | 9 | 1 | 4993024 | 47242 | 16 | 46 | 89 |

Reproduce: `make fleet-poc` (MinIO) or `cargo run --release -p server --bin tiny-eye-fleet` (local disk). One run on one machine, under whatever else the machine was doing; compare the viewer with the baseline of the same run.

