---
title: Testing and evidence
description: Every test layer in tiny-eye, how to run it, what it proves, the latest results, and how to add a conformance case.
sidebar_position: 13
---

tiny-eye's rule is "proven, not claimed": each behaviour has a test that can fail, and the main suites write their results as Markdown reports under `docs/deep-dive/evidence/`.

## Test layers

| Layer | Where | Run with | Proves |
| :--- | :--- | :--- | :--- |
| Unit tests | `#[cfg(test)]` modules in `crates/*/src` | `make test` | Config layering, typo refusal and redaction; receivers (remote-write round trip, staleness markers, OTLP JSON, redaction, Loki); log masking; TQS validator rules; dashboard save/load; auth scope table; MCP tool schemas |
| Proof of concept | `crates/engine/tests/poc.rs` | `make poc` (also in `make test`) | The storage assumptions the engine builds on, on this machine |
| Node integration | `crates/engine/tests/node.rs` | `make test` | The write path end to end, durability and overload |
| Conformance | `crates/tqs/tests/conformance/` (needs the `mutants` feature, so not part of `make test`) | `make conformance` | TQS equals Prometheus 3.16 point by point; spans/logs/traces equal an independent computation; validation codes; a mutation check |
| End to end | `crates/server/tests/e2e.rs` | `make e2e` (also in `make test`) | The whole binary equals Prometheus on remote-written data spread over every storage state |
| OpenAPI drift | `crates/server/tests/openapi.rs` | `make test` | The TQS JSON Schema and the Rust validator agree |
| Schema golden | `crates/server/tests/schemas.rs` | `make test` | Generated grammars are current; config and dashboard files match their schemas |
| Dashboards round trip | `crates/server/tests/dashboards.rs` | `make test` | UI edits round-trip through the API and the YAML file |
| Benchmark | `crates/server/src/bin/bench.rs` | `make bench`, `make bench-s3` | Ingest, write path, query and trace-lookup numbers for one machine |
| Fleet proof of concept | `crates/server/src/bin/fleet.rs` | `make fleet-poc` | Independent nodes, each with its own catalog, give the same answers as one node when read together |
| Profile (manual) | `crates/server/tests/profile.rs` | `cargo test --release -p server --test profile -- --ignored --nocapture` | Prints DuckDB `EXPLAIN ANALYZE` for a heavy panel; not a behaviour test |

`make tools` must have run once: the tests load the pinned DuckDB extensions from `.tools/duckdb_ext`, and the conformance and e2e suites start the pinned Prometheus from `.tools/`.

### Proof of concept (`poc.rs`)

| Test | Proves |
| :--- | :--- |
| `p1_writer_files_register_in_ducklake` | Files from the generated schema and the Rust Parquet writer register with `ducklake_add_data_files` (MAP, BLOB, TIMESTAMP_NS), and a schema mismatch is refused |
| `p2_hot_tail_counted_once` | Hot-tail rows are counted once before flush, after commit and after release |
| `p3_compaction_swap_and_p4_retention` | The compaction swap (`DELETE ... WHERE filename IN` + add L1 in one transaction) keeps rows unchanged; retention makes DuckLake delete expired objects |
| `p5_tqs_on_lake_views` | TQS runs unchanged on the lake plus hot-tail views |
| `p6_bloom_on_trace_id` | A bloom filter on `trace_id` is present in every row group |
| `p7_arrow_appender_round_trip` | The Arrow appender keeps column types |

### Node integration (`node.rs`)

| Test | Proves |
| :--- | :--- |
| `ingest_flush_checkpoint_compact_retain` | The full cycle: the same `rate` result from the hot tail only, after flush (still hot), after checkpoint (counted once), and from the lake only; then compaction (fewer files, series deduplicated) and retention (everything dropped) |
| `wal_replay_after_crash_loses_nothing` | A crash without a flush loses no acknowledged sample (WAL replay) |
| `arena_limit_refuses_with_overload` | Above `arena_limit`, ingest is refused with overload (`429`) |
| `spans_and_logs_round_trip_through_the_lake` | Spans and logs survive flush and checkpoint and are queryable |
| `failed_flush_is_retried_and_loses_nothing` | A failed flush is retried in order and loses nothing |
| `abandoned_queries_return_their_connection` | A query whose client went away still returns its pooled connection |
| `retries_do_not_double_count_or_lose_series` | A retried request does not double count and does not lose series rows |

### Conformance (`make conformance`)

The suite in `crates/tqs/tests/conformance/` builds deterministic, Margince-shaped fixtures (`fixtures.rs`: AI-call counters, a latency histogram, queue-depth gauges, spans and logs) with planted edge cases: a counter reset, a series that starts late, one that stops, missing scrapes, jittered timestamps, a label value with a quote, a provider with no error series, a histogram whose p99 falls in the `+Inf` bucket, a histogram reset, a trace with a lost parent span.

- **Metric cases** import the fixtures into a Prometheus 3.16 TSDB (`promtool tsdb create-blocks-from openmetrics`), run each case's PromQL there, run the TQS translation on DuckDB, and compare every point (relative tolerance 1e-9; a point on one side only fails).
- **Real dashboard panels** repeat this for 11 panel shapes from a real board over three namespaces (including an empty one).
- **Deliberate divergences** D1 (`fill: zero` on a missing operand) and D2 (division by zero is a gap) assert the documented difference.
- **Spans, logs and traces** compare against an independent Rust computation over the same fixtures.
- **Validation** cases must be refused with a specific problem code; A1 and A2 check that SQL-looking input is bound as data.
- **Mutation check:** built with `--features mutants`, the suite re-runs with each of 16 mutants, each breaking one semantic rule in the compiler (for example `closed_window`, `no_extrapolation`, `division_ieee`, `time_in_ms`). Every mutant must turn at least one case red; an uncaught mutant fails the suite.

`make conformance` writes [TQS coverage](evidence/tqs-coverage.md).

### End to end (`make e2e`)

`e2e.rs` sends the same samples (114 series) with Prometheus remote-write to a real Prometheus 3.16 and to the tiny-eye HTTP API, then compares 20 panels at every step: Prometheus with the panel's PromQL, tiny-eye with the TQS translation. During the run the data ends up in every storage state at once: compacted L1 files, L0 files, a flushed but not yet applied envelope (rows still in the hot tail), and rows only in RAM. Planted cases include a counter reset, a late series, a series ending with a staleness marker, 3 % missing scrapes, 0 to 2 s jitter, a histogram, `label_replace` input, and selections that match nothing (`or vector(0)`, `absent`). It writes [End to end](evidence/e2e.md).

### Contract tests

- `openapi.rs`: every example in `api/examples/` and every panel in `dashboards/` (with dashboard vars applied) passes both the TQS JSON Schema from `api/openapi.yaml` and the Rust model plus validator; eight shape errors fail both.
- `schemas.rs`: everything `make schemas` would write equals the committed file; `config/default.yaml` and `config/dev.yaml` match the config schema; every file in `dashboards/` matches the dashboard schema.
- `dashboards.rs`: load the demo `shop.yaml`, `GET`, change layout and order, `PUT`, check the YAML file and a fresh load; invalid states (width 13, bad step, unknown `ui` key, mismatched id) are refused with paths and nothing is written.

See [API and code generation](api-and-codegen.md).

### Benchmark (`make bench`, `make bench-s3`)

`tiny-eye-bench` runs a real node in process behind its HTTP API: a remote-write backfill with 4 concurrent clients, flush and checkpoint, panels on L0, compaction, panels on L1, panels over the hot tail, then OTLP traces and trace lookups by id before and after compaction (with S3, GET requests are counted from DuckDB's HTTP log). `make bench-s3` starts MinIO on port 19000 first (`make s3-up`). Reports: [local disk](evidence/bench-local.md), [MinIO](evidence/bench-s3.md).

### Where the benchmarks run (`make bench-all`)

`make bench-all` runs `make bench`, `make bench-s3` and `make fleet-poc` one after the other, even under `make -j`, so they don't compete for CPU. It needs Docker for MinIO.

- **Every Monday on GitHub Actions.** The `bench` workflow (`.github/workflows/bench.yml`) runs `make bench-all` on a GitHub-hosted `ubuntu-24.04` runner. Each run gets the same runner class (4 vCPUs, 16 GB RAM), but the CPU model can differ between runs, so compare trends rather than single numbers; the run block records the CPU. The workflow uploads the reports as a run artifact, opens a pull request with them, merges it itself, then starts `pages.yml` to redeploy the site. If a report records a broken equality (materialized points differ from raw, or fleet results differ from one node), the benchmark exits with an error: the run fails, nothing is merged, and the artifact keeps the failing report. It runs only on `main`, because the reports land there. You can also start it by hand from the Actions tab (**Run workflow**).
- **On your machine.** `make bench-all` writes the same reports, stamped `local machine`. Don't commit them: the weekly run owns these files. `git restore docs/deep-dive/evidence` discards a local run.

Every report starts with a run block, the record of that run. A local run:

```text
> **Run:** 2026-11-28 10:30 UTC · commit `448c00e9bf` · local machine · 8 logical CPUs (Apple M1 Pro), 16 GB RAM, macos
```

A run on GitHub Actions replaces `local machine` with the runner image and a link to the run, for example ``GitHub Actions, `ubuntu24` 20261124.1 runner ([run 123…](…/actions/runs/123…))``. That link leads to the run's logs and the uploaded reports (GitHub keeps both for 90 days), which is what lets anyone check the committed numbers. `(uncommitted changes)` after the commit means the code had local edits when the benchmark ran; reports written earlier in the same run don't count.

The workflow needs two things from the repository:

- **Settings → Actions → General → Allow GitHub Actions to create and approve pull requests**, or opening the pull request fails.
- `main` must accept that pull request without required reviews or status checks. A pull request opened by the workflow starts no CI, so a required check would never report and the merge step would fail.

## What CI runs

`.github/workflows/ci.yml` runs on pull requests and pushes to `main`, on Ubuntu with stable Rust (a newer push to a pull request cancels its older run; only `main` saves the Rust build cache, which pull requests restore): `make tools`, `make lint` (rustfmt check, clippy with warnings as errors), `make test`, `make conformance`. `make check` runs `lint`, `test` and `conformance` locally. The benchmarks don't run on pushes or pull requests; the separate `bench` workflow runs them weekly (see [Where the benchmarks run](#where-the-benchmarks-run-make-bench-all)).

## Latest results

| Suite | Result | Report |
| :--- | :--- | :--- |
| Conformance | 105/105 cases pass: 34 metric cases, 2 deliberate divergences, 17 real-panel cases, 6 spans, 5 logs, 3 traces, 38 validation and injection cases; 16/16 mutants caught | [TQS coverage](evidence/tqs-coverage.md) |
| End to end | 20/20 panels equal to Prometheus | [End to end](evidence/e2e.md) |
| Benchmark, local disk | about 1.65 M samples/s remote-write; 6 h `sum by (rate)` panel about 0.58 s p50 | [Local disk](evidence/bench-local.md) |
| Benchmark, MinIO | same ingest rate; absent trace id needs 2 GETs after compaction (22 before) | [MinIO](evidence/bench-s3.md) |
| Fleet proof of concept, MinIO | 9/9 queries equal to one node in 6 read setups (0, 50, 150 ms round trip × catalog copies read remotely or downloaded); downloading catalog copies refreshes in 0.25 s at 50 ms round trip | [Fleet](evidence/fleet-poc.md) |
| PoC, node, contract tests | pass (run by `make test`) | none |

The benchmark figures in this table come from one laptop run; the linked reports hold the latest weekly run.

The evidence pages are generated; rerun the target to refresh them.

## Add a conformance case

Metric cases live in `metric_cases()` in `crates/tqs/tests/conformance/suite.rs`. Each is a `MetricCase` with an id, a description, the TQS document, the output to compare and the reference PromQL. Helpers build the document: `ts(queries, formulas)` wraps queries in a `timeseries` document over the suite's range, `mq(name, metric, matchers, fn, agg)` builds a metrics query, `m(label, op, value)` a matcher.

1. Pick the next free id (`M34` after `M33`).
2. Add an entry, for example:

   ```rust
   single(
       "M34",
       "max by of rate",
       mq("a", CALLS, json!([]), json!({"op": "rate", "window": "5m"}), json!({"op": "max", "by": ["provider"]})),
       "max by (provider) (rate(margince_ai_calls_total[5m]))",
   ),
   ```

   Use `c(id, function, doc, output, promql)` instead of `single` when the document has several queries or formulas. For a `table` case evaluated at the end of the range, build the `MetricCase` directly with `instant: true` (see `M30`).
3. If the case needs data the fixtures do not have, add it to `fixtures.rs` and name the planted edge case in its header comment.
4. Run `make conformance`. The case must pass, and the report is rewritten.
5. If the case protects a rule no case covered before, consider a mutant: add it to `tqs::MUTANTS` in `crates/tqs/src/lib.rs` and guard the broken branch in the compiler with `mutant("name")`; the suite then proves the case turns red.

Validation cases live in `validation_cases()` (an id, the expected problem code, and a document built with `with(|j| ...)` from a valid base); spans, logs and trace cases in `run_event_cases()`.
