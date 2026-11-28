---
title: Limitations and roadmap
description: What tiny-eye does not do yet, where it deviates from the specification, the decisions taken on the spec's open questions, and what comes next.
sidebar_position: 14
---

tiny-eye implements the specification's milestone M1 (single-node metrics) and most of M2 (traces, logs, agents, auth) as one binary. In place of the spec's cluster (M3), several nodes can share one bucket as a [fleet](../examples/run-a-fleet.md) and answer queries over each other's data. This page lists what is missing, where the implementation knowingly differs from the spec, and the decisions taken on the spec's open questions.

## Not built, by milestone

### M2: traces, logs, agents

- `traces_index` (a per-trace summary table for faster trace search).
- A read-only laptop / agent mode. A laptop can instead run a normal node in the [fleet](../examples/run-a-fleet.md) and read the published catalog copies. It is a full node: anything sent to it is written under its own prefix, and it needs read, write and delete access like any node. Running it with a read-only storage key is not tested.
- MCP over streamable HTTP; only the stdio bridge (`tiny-eye mcp`) exists.
- The MCP tools `get_trace_waterfall`, `search_logs`, `correlate_anomalies` and `save_panel`. The first two are covered by `run_tqs` with `kind: trace` and `kind: log_search`.
- Span events and links are not stored.

### M3: cluster, replaced by the fleet

The spec's cluster (separate roles, a leased coordinator per region, a hot-tail RPC) is not built. A **fleet** is built instead: every node is a full `--role all` node with its own catalog and object prefix, publishes a copy of its catalog to the shared bucket, and queries the copies of the others ([Run a fleet](../examples/run-a-fleet.md), proven by `crates/engine/tests/fleet.rs`). What that leaves out, on purpose or not yet:

- **Other nodes' hot tails are not visible.** A node sees another node's rows only after they are flushed, checkpointed, published and refreshed: up to about 2 minutes with the defaults. There is no hot-tail RPC.
- **One bucket per fleet.** Every node shares one `storage.url`. Separate buckets per region (and their secrets) are not supported.
- **No coordinator.** No roles, lease or fencing; nothing checks the fleet as a whole. Duplicate `node.id`s are detected only through the manifests: the newest process wins, and the older one stops publishing and logs an error. A node with `fleet.publish_interval: 0s` writes no manifest, so a clash with its id is never detected.
- **No deduplication across nodes.** A sample retried to two nodes of one region is kept twice; duplicates are removed only within one node's files.
- **Mixed builds.** A catalog copy whose tables or columns differ from the reader's schema is left out of its answers, so during a rolling upgrade that changes the schema, nodes on different builds do not see each other's data.
- **Local disk across hosts** works only with a shared mount at the same absolute path on every host: the copies name absolute file paths.
- **No region label.** Senders must label their region or cluster on metrics, as with Prometheus `external_labels`.
- **A stopped node drops out.** Its data leaves the others' answers once its manifest is older than `coordinator.snapshot_grace` (10 minutes); its files stay in the bucket.
- **Materialized panels keep a departed node's rows.** A node joining makes stored steps recompute; a node leaving does not, so steps already stored keep its rows until retention drops them.
- `replicated` durability. A `postgres:` catalog URL is accepted only on a node alone (both `fleet.*_interval` set to `0s`); a catalog shared by several nodes is not built.
- OTLP over gRPC and Loki protobuf push (only OTLP/HTTP and Loki JSON push exist).
- mTLS between nodes (nodes do not talk to each other; they only share the bucket).

### M4: dashboards and alerts

- The alert evaluator and Alertmanager integration.
- The rest of materialized panels: the spec's mergeable partials (spans and logs measures, percentiles across regions) and alert rules on the same state. Phase 1 is built (see [below](#materialized-panels)).
- A Grafana data source plugin. tiny-eye ships its own web UI instead (see [Dashboards and the web UI](dashboards-and-ui.md)).
- Dashboard variables with allowed values (options from a query). Variables are plain defaults today.

### M5: multi-region

- Per-region buckets and secrets, catalog replicas, residency (`in-region-only`). `node.region` is stamped on rows and names the node's object prefix, and a fleet can have nodes in several regions sharing one bucket; `partial_regions` is always empty.

### Not tied to a milestone

- Native histograms. OTLP delta temporality and exponential histograms are counted and skipped.
- Rollups (5-minute and 1-hour) and their retention; only raw retention exists. Compaction writes `positive_delta` into L1 files, but no query reads it.
- A general query result cache. Only [materialized panels](materialized-panels.md) keep results; every other query is evaluated from raw rows.
- Erasure (rewrite files to remove data).
- `bytes_scanned` in responses, per-query bytes-scanned limits, and some self-metrics (commit lag, compaction backlog, cache hits).
- Multi-tenant isolation (one tenant per node).
- Dependency licence checks in CI.

## Materialized panels

The spec's materialized panels (section 9.1, D5, D6, milestone M4) are **built as phase 1**: a
panel with `materialize: true` keeps its computed points in a node-local DuckDB file, a background
task extends them as data settles, late samples trigger a recompute, and reads take stored points
plus computed head and tail — equal to the raw query at the same timestamps
([Materialized panels](materialized-panels.md), [benchmark](evidence/bench-local.md#materialized-panels)).

Limits of phase 1, by design:

- **No backfill.** A panel's points start when it is first seen; older steps are computed on read.
- **Shapes.** Timeseries metric queries whose points depend only on their own timestamp: not
  `table`, `fill: zero`, `top` ranked over the range, `$__range`/`$__interval`, or spans/logs
  measures.
- **Node-local.** The store belongs to one node. In a fleet a node's panels include other nodes' rows, and stored steps are recomputed when another node's late rows, a joining node or a returning node become visible.

Not built from section 9.1: the mergeable `view_partials` state fed from flush envelopes, which
would add spans and logs measures and percentiles merged across regions (log-bucket histograms,
D6), and alert rules evaluated on the same state.

## Known deviations

From `README.md` and the code:

- **Response fields.** `freshness_s` is the age of the oldest other-node catalog copy read (0 on a node alone), not the spec's "seconds since the newest visible row"; `partial_regions` is always empty; `bytes_scanned` is missing; `elapsed_ms` and `rows` are additions.
- **Result labels** are parsed from the series key, so a label value containing `,` is split ambiguously (the `key` is exact).
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
| Q19 | Laptop mode: snapshot export or read-only PostgreSQL role? | Snapshot export + CI gate | Resolved by the fleet: every node publishes a gzipped copy of its catalog (a snapshot export) and any node, a laptop included, attaches the others' copies. The copy uses DuckLake's internal metadata database name; `ducklake_metadata_database_keeps_its_name` in `crates/engine/tests/fleet.rs` is the CI gate |
| Q20 | Wired test target: local stack only, or also shared? | Local stack | E2E and conformance run locally on synthetic, Margince-shaped data |

## Roadmap

A short, ordered view of what is next, based on the gaps above:

1. **Web UI.** Finish the Preact UI (dashboards, Explore, traces, logs) and a build target that embeds it into the binary.
2. **Beyond the fleet.** OTLP gRPC; `replicated` durability; a read-only laptop node with a read-only storage key. The fleet replaced the spec's cluster roles, lease and hot-tail RPC.
3. **Alerts and materialized panels (M4).** Evaluator with Alertmanager v2; the [partials design for materialized panels](#materialized-panels) fed from ingest envelopes into `view_partials`, read by both panels and alert rules (phase 1, stored panel points, is built).
4. **Operability.** Missing self-metrics, `bytes_scanned`, a published container image, licence checks in CI.
5. **Multi-region (M5):** separate buckets per region and residency.

See [Testing and evidence](testing-and-evidence.md) for what is proven today.
