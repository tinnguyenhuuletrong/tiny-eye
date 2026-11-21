# PoC results

DuckDB v2.0.0 + DuckLake (PostgreSQL 16 catalog) + MinIO; Rust writer (parquet 60, object_store 0.14).

**9/9 experiments passed.**

## E1 — PASS — Object storage (MinIO) gives the conditional writes the commit queue and lease rely on

- create-if-absent twice: second refused = true
- renew with current ETag accepted = true
- update with stale ETag refused (412) = true
- 8 concurrent create-if-absent on one lease key: 1 winner(s)

## E2 — PASS — Ingest writes sorted ZSTD Parquet + commit envelopes with no DuckDB and no routing

- 200000 spans and 144000 counter samples generated; 178867 spans and 131937 samples written
- 11 envelopes, 44 Parquet files PUT to 2 regional buckets in 772.9 ms (includes 3 span layouts for E4)
- crash of us-east-n1 in window 1 lost 21133 spans and 12063 samples (unflushed batch; durability mode `memory`)
- bytes: spans 7532942 / spans_nobloom 7640763 / spans_v5 8411439 / metrics_points 816010
- metrics L0 bytes/sample = 6.18 (small sprayed files, ~1,000 samples each)
- probe trace 03879a43aac88a9fa76c88f4fc8cd4d0 crosses both regions and a flush boundary

## E3 — PASS — Checkpointer registers external files from two buckets atomically and idempotently

- pass 1 applied 11 envelopes; commit latency p50 41.2 ms, max 49.5 ms (4 files each, PostgreSQL catalog)
- pass 2 (after a simulated crash before envelope delete): applied 0, skipped 1 — no duplicate rows
- lake rows: spans 178867 (expected 178867), metrics_points 131937 (expected 131937), regions 2
- envelope with a missing file: error = true, files 11 -> 11, marker rows 0

## E4 — PASS — Trace lookup by id across regions and flush boundaries, pruned by sort + bloom

- spans          5/5 spans from 2 regions; 26 GETs, 294.7 KiB read, 23.9 ms over 11 files
- spans_nobloom  5/5 spans from 2 regions; 22 GETs, 623.8 KiB read, 38.5 ms over 11 files
- spans_v5       5/5 spans from 2 regions; 38 GETs, 509.6 KiB read, 52.2 ms over 11 files
- absent trace id on `spans`: 0 rows; 22 GETs, 125.3 KiB, 19 ms
- waterfall: us-east gateway POST /api/checkout 3693.7ms (root) -> us-east checkout checkout.place 95.2ms -> us-east checkout SELECT orders 2.6ms -> eu-west payment payment.charge 39.1ms -> eu-west ledger ledger.post 14.9ms

## E5 — PASS — Counter increase needs no ingest-side state: computed at read (L0) and by the compactor (L1)

- 4000 series; true total increase 2722890
- read-time lag over L0: total 2722386 (-0.0185 %); series differing from best-possible: 0
- compactor L1 `positive_delta`: series differing from best-possible: 0; L1 4.60 bytes/sample; compaction 188.6 ms
- series that lost increase physically (a reset inside the crashed batch): 15
- v5 ingest-state rule under the same crash: total 2468824 (-9.3307 %)

## E6 — PASS — Own compactor swaps L0->L1 atomically; change feed is unsafe as a view source

- rows before/after swap: 178867/178867; 11 L0 files replaced by 2 L1 files
- probe lookup: 26 GETs / 294.7 KiB before, 6 GETs / 155.6 KiB / 13.4 ms after
- change feed over the swap: 178867 insertions + 178867 deletions for 0 net new rows
- after expire_snapshots + cleanup_old_files: 0/11 externally written L0 objects still in MinIO

## E7 — PASS — Hot-tail Arrow batch unions with the lake; per-node watermark prevents double count

- hot batch: 60000 spans (node us-east-n0 epoch 1 seq 4)
- A before flush (hot only): 238867 (expected 238867), query 120.7 ms
- B committed, still retained in RAM: 238867 (expected 238867), query 6.3 ms
- B without the watermark (naive UNION ALL): 298867 — double counts 60000
- C dropped from RAM: 238867 (expected 238867); lake-only count 8.5 ms

## E8 — PASS — Mergeable percentiles: log-bucket partials merged with SUM in plain SQL

- 1339 partial rows from 3 files x 4 services; parquet files for partials: 0 (0 = inlined in the catalog)
- p50/p95/p99 for 4 services, merged across regions and files: worst relative error 1.01 % vs exact
- panel read (merge + quantiles) over inlined partials: p50 11.5 ms, max 13 ms (20 runs)

## E9 — PASS — Laptop/agent mode: read-only DuckLake catalog snapshot attached straight from S3

- inlined rows must be flushed first (PostgreSQL stores them in backend-specific types): partial files 0 -> 1
- snapshot = 28 metadata tables copied from PostgreSQL into one 3682304-byte DuckDB file, PUT to MinIO
- fresh process, no PostgreSQL: attach 45.2 ms; trace lookup 5/5 spans from 2 regions in 27.9 ms
- counter total equals the cluster's: true
- write attempt refused: true

