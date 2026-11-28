# Share-nothing fleet — proof of concept

> **Run:** 2026-11-28 12:32 UTC · commit `e62b87c686` · GitHub Actions, `ubuntu24` 20261122.23.1 runner ([run 46142991662](https://github.com/tinnguyenhuuletrong/tiny-eye/actions/runs/46142991662)) · 4 logical CPUs (Intel(R) Xeon(R) 6975P-C), 15 GB RAM, linux

Every node keeps its own DuckLake catalog and object prefix and publishes a copy of its catalog; a read-only viewer attaches all copies and queries them as one. Compared against a baseline node that holds every region's data in one catalog. Storage: MinIO on the same host (S3 API, ~1 ms per request). DuckDB v2.0.1.

## Data

3 regions, each 745 series (500 counters, 125 gauges, 120 histogram buckets) over 6 h at 15 s, and 6000 traces × 6 spans: 3218400 samples and 108000 spans in total. Every region's series carry `cluster=<region>`. The baseline node received the same requests. All nodes flushed, checkpointed and compacted (one L1 file per table and hour).

## Publish (each node, after compaction)

| Node | Copy ms | Gzip ms | PUT ms (both) | Snapshot bytes | Gzipped bytes |
| :--- | ---: | ---: | ---: | ---: | ---: |
| region-0 | 24 | 1 | 20 | 5255168 | 43310 |
| region-1 | 26 | 2 | 19 | 5255168 | 43228 |
| region-2 | 25 | 2 | 20 | 5255168 | 43248 |

## Viewer: attach and refresh (3 snapshots)

The viewer reaches object storage through a TCP proxy that delays each direction by half the round trip (chunks stay pipelined, bandwidth is not limited): a laptop that far from the buckets.

| Round trip | Mode | First download ms | First attach + views ms | Refresh download ms | Refresh attach + swap ms |
| ---: | :--- | ---: | ---: | ---: | ---: |
| 0 ms | attached from object storage | — | 115 | — | 101 |
| 0 ms | downloaded, attached locally | 14 | 73 | 11 | 72 |
| 50 ms | attached from object storage | — | 1695 | — | 1696 |
| 50 ms | downloaded, attached locally | 177 | 74 | 171 | 72 |
| 150 ms | attached from object storage | — | 4701 | — | 4694 |
| 150 ms | downloaded, attached locally | 487 | 75 | 469 | 75 |

## Queries: viewer against the baseline node

Every query ran on the baseline node (one catalog with every region's data, next to the bucket) and on the viewer (UNION ALL over 3 attached catalogs). "First" runs with DuckDB's caches empty (counting the previous query's GETs switches them off, which empties them), so it is what a panel costs the first time; p50 is over 10 warm runs. Results equal to the baseline on every viewer configuration: **yes** (relative tolerance 1e-9; trace rows compared without the `region` column, which the viewer reports per source region and the baseline as `all`).

| Query | Rows | GETs, caches off: baseline / viewer | Baseline p50 ms | 0 ms, remote: first / p50 | 0 ms, downloaded: first / p50 | 50 ms, remote: first / p50 | 50 ms, downloaded: first / p50 | 150 ms, remote: first / p50 | 150 ms, downloaded: first / p50 |
| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `sum by (job) (rate(http_requests_total[5m]))` | 3510 | 47 / 84 | 809.7 | 900 / 775 | 897 / 784 | 1949 / 771 | 1143 / 772 | 3983 / 788 | 1750 / 785 |
| `topk(10, sum by (route) (rate(…[5m])))` | 3510 | 47 / 84 | 827.2 | 795 / 797 | 782 / 786 | 1563 / 788 | 1626 / 793 | 3129 / 802 | 2977 / 796 |
| `error ratio formula (2 queries)` | 1053 | 94 / 168 | 846.5 | 849 / 883 | 833 / 881 | 1592 / 884 | 1626 / 877 | 3315 / 895 | 3330 / 886 |
| `histogram_quantile(0.99, … by (job, le))` | 3510 | 47 / 84 | 211.3 | 278 / 226 | 277 / 222 | 1002 / 226 | 986 / 222 | 2554 / 235 | 2538 / 225 |
| `max by (job) (gauge)` | 3510 | 47 / 84 | 142.6 | 173 / 156 | 200 / 152 | 851 / 152 | 841 / 152 | 2161 / 162 | 2157 / 153 |
| `one series: rate(…{instance="10.0.0.1:9100"})` | 1053 | 38 / 101 | 33.7 | 61 / 49 | 58 / 47 | 741 / 47 | 736 / 47 | 2053 / 54 | 2042 / 48 |
| `table: sum by (code) (increase(…[$__range]))` | 2 | 47 / 84 | 556.0 | 588 / 552 | 580 / 580 | 1373 / 557 | 1349 / 558 | 2871 / 561 | 2874 / 566 |
| `trace by id, present` | 6 | 15 / 14 | 15.8 | 37 / 27 | 37 / 25 | 193 / 26 | 196 / 26 | 497 / 27 | 496 / 25 |
| `trace by id, absent` | 0 | 2 / 6 | 11.5 | 30 / 24 | 28 / 23 | 189 / 24 | 190 / 24 | 493 / 24 | 493 / 25 |

## Freshness: one new sample from region-0 to the viewer (no added round trip)

| Step | ms |
| :--- | ---: |
| Flush (Parquet + PUT + envelope) | 14 |
| Checkpoint (register in the node's catalog) | 29 |
| Publish: copy catalog + gzip | 30 |
| Publish: PUT | 21 |
| Viewer refresh: download 3 snapshots | 15 |
| Viewer refresh: attach + swap views | 80 |
| **Work on the path** | **189** |

Visible on the viewer before: no; after: yes. The lag a user sees is set by the intervals, not this work: at most `flush_interval` + `checkpoint_interval` + publish interval + viewer refresh interval.


## Catalog growth on one node (240 flush + checkpoint cycles, each one scrape of every series)

| After | Files in catalog | DuckLake snapshots | Snapshot bytes | Gzipped bytes | Copy + gzip ms | PUT ms | Viewer refresh ms |
| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 0 flushes | 11 | 17 | 5255168 | 43645 | 21 | 21 | 91 |
| 60 flushes | 131 | 77 | 5255168 | 52116 | 25 | 19 | 100 |
| 120 flushes | 251 | 137 | 5255168 | 58912 | 25 | 19 | 106 |
| 180 flushes | 371 | 197 | 5255168 | 65380 | 26 | 21 | 102 |
| 240 flushes | 491 | 257 | 5255168 | 73688 | 27 | 20 | 104 |
| then compaction + snapshot expiry | 9 | 1 | 4993024 | 47142 | 25 | 19 | 101 |

Reproduce: `make fleet-poc` (MinIO) or `cargo run --release -p server --bin tiny-eye-fleet` (local disk). One run on one machine, under whatever else the machine was doing; compare the viewer with the baseline of the same run.

