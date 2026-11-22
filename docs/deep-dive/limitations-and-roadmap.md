---
title: Limitations and roadmap
description: What tiny-eye does not do yet, where it deviates from the specification, the decisions taken on the spec's open questions, and what comes next.
sidebar_position: 14
---

tiny-eye implements the specification's milestone M1 (single-node metrics) and most of M2 (traces, logs, agents, auth) as one binary in single-node mode. This page lists what is missing, where the implementation knowingly differs from the spec, and the decisions taken on the spec's open questions.

## Not built, by milestone

### M2: traces, logs, agents

- `traces_index` (a per-trace summary table for faster trace search).
- Laptop / agent mode: snapshot export of the catalog for local querying (E9).
- MCP over streamable HTTP; only the stdio bridge (`tiny-eye mcp`) exists.
- The MCP tools `get_trace_waterfall`, `search_logs`, `correlate_anomalies` and `save_panel`. The first two are covered by `run_tqs` with `kind: trace` and `kind: log_search`.
- Span events and links are not stored.

### M3: cluster

- Roles other than `--role all`, lease and fencing, the hot-tail RPC between nodes, and `replicated` durability. A `postgres:` catalog URL is accepted, but multi-node operation is not built.
- OTLP over gRPC and Loki protobuf push (only OTLP/HTTP and Loki JSON push exist).
- mTLS between nodes.

### M4: dashboards and alerts

- The alert evaluator and Alertmanager integration.
- The rest of materialized panels: the spec's mergeable partials (late rows, spans and logs measures, percentiles across regions) and alert rules on the same state. Phase 1 is built (see [below](#materialized-panels)).
- A Grafana data source plugin. tiny-eye ships its own web UI instead (see [Dashboards and the web UI](dashboards-and-ui.md)).
- Dashboard variables with allowed values (options from a query). Variables are plain defaults today.

### M5: multi-region

- Per-region buckets and secrets, catalog replicas, residency (`in-region-only`). `node.region` is stamped on rows and envelopes, but one node serves one region; `partial_regions` is always empty.

### Not tied to a milestone

- Native histograms. OTLP delta temporality and exponential histograms are counted and skipped.
- Rollups (5-minute and 1-hour) and their retention; only raw retention exists. Compaction writes `positive_delta` into L1 files, but no query reads it.
- A general query result cache. Only [materialized panels](materialized-panels.md) keep results; every other query is evaluated from raw rows.
- Erasure (rewrite files to remove data).
- `bytes_scanned` in responses, per-query bytes-scanned limits, and some self-metrics (commit lag, compaction backlog, cache hits).
- Multi-tenant isolation (one tenant per node).
- Dependency licence checks in CI.

## Materialized panels

**Built (phase 1):** a dashboard panel with `materialize: true` keeps its computed points in a
node-local DuckDB file, the background loop extends them as data becomes older than
`materialize.settle`, and a read uses the stored points and computes only the head before the
coverage and the unsettled tail. The answer equals the raw query on the same grid
(`crates/engine/tests/materialize.rs`); on the benchmark's 6-hour panels reads are up to 13×
faster ([Materialized panels](materialized-panels.md), [benchmark](evidence/bench-local.md#materialized-panels)).

Limits of phase 1, by design:

- **No backfill.** A panel's points start when it is first seen; earlier steps are computed on
  every read until they fall out of the range.
- **Late samples.** A sample that arrives later than `settle` after its timestamp is not in the
  steps already stored.
- **Shapes.** Timeseries metric queries whose points depend only on their own timestamp. Not
  `table`, `fill: zero`, `top` ranked over the range, `$__range`/`$__interval`, or spans/logs
  measures.
- **Node-local.** The file belongs to one node; there is one node today.

**Designed, not built:** the spec (section 9.1, D5, D6, milestone M4) stores mergeable partials
per time bucket, fed from each flush envelope (never the DuckLake change feed, which reports a
compaction as N inserts plus N deletes), with log-bucket histograms merged by `SUM`. That
design handles late rows, spans and logs measures and percentiles across regions, and would
also feed an alert evaluator. Open points: how the hot tail joins the partials and how partials
follow retention.

## Known deviations

From `README.md` and the code:

- **Response fields.** `freshness_s` is always 0 (every read includes the hot tail); `partial_regions` is always empty; `bytes_scanned` is missing; `elapsed_ms` and `rows` are additions.
- **Result labels** are parsed from the series key, so a label value containing `,` is split ambiguously (the `key` is exact).
- **OTLP points** flagged "no recorded value" are stored as values.
- **`kind: trace` without `time`** probes every span file; pass a time range (the spec assumes one).
- **Duplicates.** Samples with the same series and timestamp (a retried request) are removed at flush and compaction (`unique` in `schema/tables.yaml`); two copies can coexist briefly in the hot tail.
- **Error text.** `500` responses carry the engine's error text; front the API with a proxy before exposing it.
- **TQS details** (see the [TQS reference](tqs-reference.md)): variables are not accepted in `step` or `offset`; `offset` is ignored on spans/logs queries; `search.trace` values do not take variables.
- **Parquet `schema_version`** is written but not checked on read.
- **Build findings.** DuckDB core must be a release build in every Cargo profile (its debug build aborts when release-built extensions free memory). DuckLake needs the `icu` extension besides `ducklake` and `httpfs`. In DuckDB 2.0.1, `epoch_ms(TIMESTAMP_NS) // n` raises a spurious cast error, so `epoch_ns` is used instead.

## Open questions and decisions

The spec (§16) lists 20 questions with proposed defaults. tiny-eye uses the proposed defaults, with one exception: for **Q7** single-node mode defaults to `wal`. The table also notes where the code shows a different or partial state.

| # | Question | Decision | State in the code |
| :--- | :--- | :--- | :--- |
| Q1 | Keep Prometheus extrapolation for `rate`/`increase`, or add an exact `increase_exact`? | Keep Prometheus semantics; add `increase_exact` in M4 if users ask | Extrapolation implemented and proven; no `increase_exact` |
| Q2 | Division by zero: gap or NaN/Inf? | Gap | Implemented (D2, mutant `division_ieee`) |
| Q3 | Spans counted by start or end time? | Start time | Implemented |
| Q4 | Event bucket `(t - step, t]` or `[t, t + step)`? | `(t - step, t]` | Implemented (mutant `bucket_closed_left`) |
| Q5 | Support subqueries? | No; change the one panel | Not supported |
| Q6 | Rewrite aggregation before the formula, or post-formula aggregation? | Rewrite before the formula | No aggregation after formulas |
| Q7 | Default durability | Single node: `wal` | `ingest.durability: wal` by default; `memory` available; `replicated` not built |
| Q8 | One tenant or isolation from M3? | One tenant | `tenant` column stamped from `node.tenant` |
| Q9 | Product and binary name? | Placeholder `blobobs` proposed | The binary and project are named `tiny-eye` |
| Q10 | S3 or R2 first? | S3 | S3 via `object_store`; benchmarked on MinIO |
| Q11 | Grafana plugin, own UI, or both? | Grafana plugin in M4 | Own web UI built (embedded in the binary); no Grafana plugin |
| Q12 | Dashboards in git or in the engine's store? | Files in git, loaded by the engine | Implemented; UI saves write the YAML file |
| Q13 | Own alert evaluator + Alertmanager, or external? | Own evaluator + Alertmanager v2 | Not built (M4) |
| Q14 | Retention: raw 15 d, 5 m rollups 90 d, 1 h rollups 13 months? | As written | Raw retention `15d` default; rollups not built |
| Q15 | Provide `$__rate_interval`? | No | Not provided |
| Q16 | Empty label value equals absent label? | Yes | Implemented (remote-write drops empty labels; matchers treat absent as `""`) |
| Q17 | Fixed, versioned log masking rules? | Yes | Fixed list, `pattern_version` 1 |
| Q18 | Limits: 10 queries, 11,000 points, regex 256, formula 512, `limit` 5,000? | Yes | Implemented |
| Q19 | Laptop mode: snapshot export or read-only PostgreSQL role? | Snapshot export + CI gate | Not built |
| Q20 | Wired test target: local stack only, or also shared? | Local stack | E2E and conformance run locally on synthetic, Margince-shaped data |

## Roadmap

A short, ordered view of what is next, based on the gaps above:

1. **Web UI.** Finish the Preact UI (dashboards, Explore, traces, logs) and a build target that embeds it into the binary.
2. **Cluster (M3).** Roles, lease and fencing, hot-tail RPC, `replicated` durability, OTLP gRPC.
3. **Alerts and materialized panels (M4).** Evaluator with Alertmanager v2; the [partials design for materialized panels](#materialized-panels) fed from ingest envelopes into `view_partials`, read by both panels and alert rules (phase 1, stored panel points, is built).
4. **Operability.** Missing self-metrics, `bytes_scanned`, a published container image, licence checks in CI.
5. **Multi-region (M5)** and laptop snapshot export.

See [Testing and evidence](testing-and-evidence.md) for what is proven today.
