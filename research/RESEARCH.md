# Research — testing and notes

This file records what was tested, how, what it showed, and what it did not show. Every number here
comes from a run of `research/scripts/run-all.sh` on 2026-11-10. The raw outputs are in
`research/results/`.

## 1. Purpose

Turn the v5 specification's ideas into facts before implementation:
1. Does the storage design work on S3-compatible storage with DuckLake? (experiments E1–E9)
2. Can our own query schema (TQS 3) give dashboards the same numbers as PromQL? (cases M, D, R)
3. Do spans, logs and traces queries return what an independent computation says? (cases S, L, T)
4. Does the schema refuse what it must, and treat hostile input as data? (cases V, A)
5. Can the test suite fail? (mutation check)
6. How much of a real dashboard does TQS cover? (the Margince Grafana board)

## 2. Environment

| Component | Version / setup |
| :--- | :--- |
| Language | Rust 1.97, edition 2024 |
| Query engine | DuckDB 2.0.0 via the `duckdb` crate 1.20000.1 (bundled), extensions `ducklake`, `httpfs`, `postgres_scanner` 2.0.0 |
| Arrow / Parquet / S3 client | `arrow` 60, `parquet` 60, `object_store` 0.14 |
| Object storage | MinIO (built from the Go module proxy), local, two buckets `telemetry-us-east`, `telemetry-eu-west` |
| Catalog | DuckLake on PostgreSQL 16 (private cluster, port 55432) |
| Reference | Prometheus 3.16 (module v0.316.0) and promtool, built from the Go module proxy |
| Machine | 4 vCPU, 15 GB RAM, local disk; MinIO and PostgreSQL on the same host |

The DuckDB extensions came from the PyPI wheels `duckdb-extension-*==2.0.0`: the public extension
host was not reachable from this machine. That is the reason for decision D12 (ship extensions with
the binary).

**Rerun:**

```bash
cd research
GOBIN=$PWD/.tools go install github.com/minio/minio@latest
GOBIN=$PWD/.tools go install github.com/prometheus/prometheus/cmd/prometheus@latest github.com/prometheus/prometheus/cmd/promtool@latest
# DuckDB extensions: see research/README.md
MARGINCE_QC=../../margince-qc scripts/run-all.sh   # last argument optional: real-board coverage
```

## 3. Method

| Evidence kind | Oracle | Why it is trustworthy |
| :--- | :--- | :--- |
| Storage experiments (E) | Ground truth kept by the generator: every span and sample, including the ones a simulated crash "lost" | The generator knows what reached storage and what did not |
| Metric cases (M, D, R) | Prometheus 3.16 on the **same** samples, imported with `promtool tsdb create-blocks-from openmetrics`; compared point by point at every step | Prometheus is the reference implementation of the semantics dashboards rely on |
| Event cases (S, L, T) | A separate Rust computation over the generated spans and logs (loops, R-7 quantile) | Independent code path from the SQL compiler |
| Validation (V, A) | Expected problem code per case; SQL-looking values must return 0 rows and leave tables intact | |
| Mutation check | 15 deliberate breaks of one rule each in the compiler | A suite that cannot fail proves nothing |
| Real-board coverage | The 99 PromQL panels margince-qc records for the Margince Grafana board | Real use, not our own choice of functions |

**Equality rule.** For every (series key, step) both sides must have a point, and the values must
agree within 1e-9 relative. The largest difference seen was 2.9e-14, from floating-point summation
order. A point present on one side only counts as a failure.

**Fixtures.** They are deterministic, seeded, and shaped like Margince's own metrics:
- AI-call counters by provider/model/task/tier/outcome;
- a latency histogram with Margince's bounds (0.005–10 s, +Inf);
- queue and pool gauges;
- the board's labels (`kubernetes_namespace`, per-workspace gauges, `declared_info`, routes, cAdvisor
  containers);
- 349 series, 161,154 samples, 14,999 spans, 4,288 logs.

Edge cases are planted on purpose and named in `research/src/fixtures.rs`:
- a counter reset, a late-starting series, a stopping series;
- 3 % missing scrapes, 0–2 s timestamp jitter;
- a label value with a quote, a provider with no error series, a flat-zero counter;
- a histogram whose p99 sits in the +Inf bucket, and a histogram reset;
- a namespace with no error series and one with no data at all;
- a trace with a lost parent span;
- non-ASCII log text.

## 4. Results

### 4.1 Summary

| Area | Result |
| :--- | :--- |
| Storage experiments E1–E9 (MinIO + DuckLake) | **9/9 pass** |
| Same TQS queries on DuckLake tables in MinIO vs in memory | **6/6 identical** (up to 667 points each) |
| Metric cases vs Prometheus (M01–M33) | **33/33 equal** |
| Deliberate divergences (D1–D2) | **2/2 behave as specified** |
| Real-board panel shapes vs Prometheus (R01–R11, 17 runs over 3 namespaces) | **17/17 equal** |
| Points compared with Prometheus, total | **30,758, all equal** |
| Spans / logs / traces vs Rust (S1–S6, L1–L5, T1–T3) | **14/14 equal** |
| Validation and injection (V01–V36, A1–A2) | **38/38** |
| Mutation check | **15/15 mutants caught** |
| Real Margince board, construct level | **98/99 panels expressible** (the 1 exception uses a subquery) |

### 4.2 Storage experiments

| ID | Claim | Key numbers |
| :--- | :--- | :--- |
| E1 | MinIO gives If-None-Match / If-Match | second create refused; stale ETag → 412; 1 of 8 racing lease writers won |
| E2 | Ingest needs no DuckDB, no routing | 200,000 spans + 144,000 samples generated, 44 files + 11 envelopes in ≈ 0.77 s; a crash lost 21,133 spans / 12,063 samples (its unflushed batch) |
| E3 | One catalog, two buckets, atomic and idempotent | commit p50 ≈ 41 ms (4 files, PostgreSQL); replay after a crash skipped 1, no duplicates; an envelope with a missing file rolled back fully |
| E4 | Trace lookup across regions and flush boundary | 5/5 spans, 2 regions; chosen layout 26 GETs / 294.7 KiB; no bloom 22 / 623.8 KiB; v5 layout 38 / 509.6 KiB; absent id 22 GETs = 2 per file |
| E5 | Counter increase without ingest state | read-time and compactor paths: 0 series off the best possible; the v5 ingest-state rule lost **9.33 %**; L1 4.60 B/sample, L0 6.18 B/sample |
| E6 | Own compactor; change feed; file ownership | 178,867 rows before and after a swap of 11 → 2 files; lookup 26 → 6 GETs; change feed reported 178,867 inserts + 178,867 deletes; DuckLake deleted 11/11 external L0 files |
| E7 | Hot tail with a per-node watermark | correct before flush, after commit, after drop; naive union double-counted 60,000; Arrow scan of 60k rows ≈ 120 ms |
| E8 | Mergeable percentiles, inlined partials | worst error 1.01 % (p50/p95/p99, scale 5); 1,339 partial rows inlined, 0 files; panel read p50 ≈ 12 ms |
| E9 | Laptop mode from an S3 catalog snapshot | attach ≈ 45 ms, lookup ≈ 28 ms, write refused; **requires flushing inlined rows first** |

### 4.3 Query schema (TQS 3)

The full per-case table is in `research/results/tqs-coverage.md`. Highlights:
- **Prometheus semantics reproduced exactly:**
  - `rate`, `increase`, `irate`, `delta` with extrapolation and reset correction;
  - the `*_over_time` family, `resets`, `changes`;
  - `last` with the 5 min lookback, including a stale series;
  - `histogram_quantile` with bucket interpolation and the +Inf edge;
  - aggregation with `by`/`without`, `count` of series, per-step `topk`, `offset`, regex and
    negative-regex matchers, value filters, scalar arithmetic, clamps, formulas over formulas.
- **Real panels.** Each was checked with the panel's own PromQL, after `$namespace` and `$__range`
  substitution:
  - stat with `or vector(0)`;
  - error % with `clamp_min`/`clamp_max`;
  - histogram p95 over the range;
  - calls per hour + the `0 * absent(…)` line;
  - queue depth with `or … * 0` baselines and two-level aggregation;
  - uptime from `time()`;
  - p95 of the 10 busiest routes (`and on(route) topk`);
  - active-model count (`count(count by … > 0)`);
  - CPU by pod with a `label_replace`-derived role;
  - declared job count.

  The namespace without error series and the namespace with no data give the same 0 / empty result
  as Prometheus.
- **Deliberate divergences:**
  - D1: `fill: zero` keeps a group that PromQL drops when one operand has no series;
  - D2: x/0 is a gap where Prometheus returns NaN.
- **Latency** (small fixtures, in memory): 5–60 ms per query. The same queries on DuckLake in
  MinIO took 28–127 ms.

### 4.4 Mutation check

| Mutant (rule broken) | Cases that turned red |
| :--- | ---: |
| no reset correction | 16 |
| closed window `[t−w, t]` | 29 |
| no extrapolation | 22 |
| lookback 4 m instead of 5 m | 3 |
| histogram returns bucket upper bound | 4 |
| `without` ignored | 1 |
| `fill` ignored | 6 |
| x/0 gives ±Inf/NaN | 1 |
| event buckets include their left edge | 1 |
| `derive` ignored | 1 |
| `then` ignored | 2 |
| outer join behaves as inner | 1 |
| `top.query` ignored | 1 |
| `absent` without labels | 1 |
| `time` in ms | 1 |

Seven rules are guarded by a single case. They are listed so that a refactor does not remove that
one case.

### 4.5 Real board coverage

`research/scripts/dashboard_coverage.py` reads margince-qc's record of the board and writes only
aggregate counts (`research/results/dashboard-coverage.md`). It reports 98/99 panels expressible and
1 needing a subquery. Constructs by panel count:

| Construct | Panels |
| :--- | ---: |
| `sum` | 71 |
| `rate` | 41 |
| `or` | 32 |
| `max` | 31 |
| `vector(0)` | 28 |
| `absent` | 16 |
| `increase` | 10 |
| `histogram_quantile` | 8 |

Every construct has a TQS equivalent and at least one verifying case.

Construct-level coverage is not numeric proof for each panel. Numeric proof exists for the 11
shapes R01–R11. The wired test in SPECIFICATION §15.3 extends it to every panel on a live Margince
scrape.

## 5. Findings that changed the design

1. **Counter deltas belong at read/compaction time, not ingest** (E5). The v5 rule loses 9.33 % in
   one crash; the read-time path loses nothing beyond physically lost resets.
2. **DuckLake's change feed cannot drive views** (E6). A compaction shows as N inserts + N deletes.
3. **DuckLake deletes files it did not write** once they expire (E6). Retention can go through it;
   double registration is forbidden.
4. **DuckDB's Parquet writer writes no bloom filter on high-cardinality columns** [P]. Span files
   need our writer.
5. **t-digest state is not storable** [P]. Log buckets are, and they merge with SUM within 1.01 %
   (E8).
6. **Laptop snapshot export must flush inlined rows first** (E9). The first attempt failed on
   PostgreSQL's backend-specific types for inlined rows.
7. **Trace-lookup cost is linear in file count** (E4, E6). Hourly compaction and time-bounded lookups
   are required.
8. **"< 1 ms" dashboard reads are not possible through a networked catalog** (E8, [P] ≈ 20 ms for 10
   inlined rows). The target is now p95 < 50 ms.
9. **PromQL's dashboard features are reproducible in a closed JSON schema with equal numbers**
   (M, R). The real board needs variables, outer joins (`or`), `absent`, two-level aggregation,
   `label_replace` and `and on … topk`. These were added as TQS constructs after reading the real
   panels, then verified.
10. **DuckDB 2.0 rejects the old `->` lambda syntax** [P]. The compiler uses `lambda x: …`.

## 6. One-off probes [P]

| Probe | Observation |
| :--- | :--- |
| DuckLake functions in 2.0.0 | 21 `ducklake_*` functions, including `add_data_files`, `rewrite_data_files`, `merge_adjacent_files`, `flush_inlined_data`, the change feed |
| `add_data_files` | registers files outside the data path; records per-file min/max |
| `DELETE … WHERE filename IN (…)` + `add_data_files` in one transaction | atomic swap; whole-file delete writes no delete file |
| `merge_adjacent_files` | not reported as inserts by the change feed, but rewrites into DuckLake-named files without our sort/bloom |
| Data inlining on PostgreSQL | small inserts write no file; reading 10 inlined rows ≈ 20 ms |
| Concurrent writers (4 × 10 inserts, one table) | 0 errors, p50 16 ms, worst 972 ms |
| DuckDB HTTP logging | counts every S3 range GET per query (used by E4) |
| `approx_quantile … EXPORT_STATE` | works in process; after Parquet it reads back as a plain STRUCT that `combine()` rejects |
| DuckDB `1/0`, `0/0` | `inf`, `nan` (IEEE, like Prometheus) |

## 7. What this research does not show

| Not shown | Why it matters | Where it is planned |
| :--- | :--- | :--- |
| Performance at production scale and on real S3 latency | MinIO on the same host has ~ms latency; fixtures are small | M1 benchmark, SPEC §15.2 |
| Durability modes other than `memory` | `wal`/`replicated` are designs only | M1 / M3 |
| Hot tail across processes | Proven in one process | M3 |
| Compaction swap racing a concurrent add | Untested | single-writer coordinator; add a test |
| `events`/`links` list-of-struct columns via `add_data_files` | Untested | first sprint |
| Native/exponential histogram queries | Only classic buckets are proven | Q-list / later |
| Materialized panels with Prometheus-extrapolated `rate` | Needs per-bucket first/last samples | M4 |
| Every one of the 99 real panels numerically | 11 shapes proven; 98 expressible | wired test, SPEC §15.3 |
| Bytes per sample on real data | Synthetic random walks | M1 with real exporter data |
| `top` ranks other than `per_step`; `bottomk` | Untested | first sprint |

## 8. History

The earlier documents are kept in `history/` for traceability:
- `v5-review.md`: the review of the v5 specification that started this work;
- `concept-v1.md`: the first PoC concept, with PromQL as a planned interface.

SPECIFICATION.md supersedes both where they disagree. The main changes are: no PromQL, TQS 3 as the
only language, and these research results.
