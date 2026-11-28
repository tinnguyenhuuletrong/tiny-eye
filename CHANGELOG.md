# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html). There are no releases yet.

## [Unreleased]

### Added

- Weekly benchmark on GitHub Actions (`.github/workflows/bench.yml`): runs `make bench-all` on a
  GitHub-hosted runner and commits the reports through a pull request it merges itself. Every
  report now opens with a run block (date, commit, machine, link to the run).
- `make fleet-poc`: proof of concept for independent nodes, each with its own catalog, read together
  from published catalog copies; report in `docs/deep-dive/evidence/fleet-poc.md`.
- Single-node engine in one Rust binary with three crates (`tqs`, `engine`, `server`).
- Ingest: Prometheus remote-write 1.0, OTLP/HTTP metrics, traces and logs (protobuf and JSON,
  gzip), Loki push (JSON); sample-age bounds, secret attribute redaction, arena limit with 429.
- Durability modes `wal` (group-committed fsync, replay on restart; the single-node default) and `memory`.
- Storage: flush per (table, hour) to sorted ZSTD Parquet with page statistics, delta and
  byte-stream-split encodings and a `trace_id` bloom filter; envelopes written with create-only PUT;
  local disk or S3.
- Checkpointer into a DuckLake catalog (idempotent via `applied_envelopes`), hot tail with a
  per-node watermark, hourly L0 → L1 compaction with `positive_delta`, series and duplicate-sample
  dedup, retention through DuckLake.
- TQS 3 query language compiled to DuckDB SQL, reproducing Prometheus semantics including staleness;
  O(series × steps) window evaluation via running sums and ASOF joins; formulas; catalog discovery.
- HTTP API generated from `api/openapi.yaml` (router, types, bearer-token scopes), validation with
  problem codes, self-metrics.
- MCP stdio bridge (`tiny-eye mcp`) with tools generated from the OpenAPI definition.
- YAML dashboards in git; a panel is a TQS query plus UI state, and `PUT` saves the UI state back
  to the YAML file.
- Web UI (Preact + Vite) embedded in the binary and served at `/`, with API types generated from
  `api/openapi.yaml`: dashboards (uPlot charts with a synced crosshair, stats with sparklines,
  tables, logs, traces; edit layout by drag and corner-resize on a 12-column grid; add panel; save
  to the YAML file), Explore (pill builder and a TQS editor validated live by the engine, synced
  both ways), trace search with waterfalls, log search with patterns and volume by level.
- Catalog discovery reports metric types and a sample of label values per label.
- `tiny-eye config` prints the effective configuration as YAML that loads back as a config file.
- Generated JSON Schemas for TQS, dashboards and config, a formula grammar and MCP tool docs
  (`make schemas`, kept in sync by a test).
- Evidence: TQS conformance against Prometheus 3.16 point by point with a mutation check
  (`make conformance`), end-to-end comparison of the whole binary against Prometheus (`make e2e`),
  storage proof of concept (`make poc`), benchmarks on local disk and MinIO (`make bench`, `make bench-s3`).
- Examples on Bun and TypeScript. Demo: a shop instrumented with OpenTelemetry (manual spans, metrics and trace propagation, Biome lint and format, `make examples-lint`), k6 load, a dashboard, ad-hoc queries and
  MCP setup for Claude Code / Claude Desktop (`make demo`, `make demo-app`, `make demo-load`).
- Container image: multi-stage `Dockerfile` (web UI, binary, pinned DuckDB extensions; non-root,
  data in a `/data` volume), `make docker`, and a release workflow publishing multi-arch images to
  GitHub Container Registry on version tags.
- Documentation site (Docusaurus, `website/`) published to GitHub Pages, and standard project files
  (license, contributing guide, security policy, code of conduct, issue and PR templates, Dependabot).
- Docs site landing page, local search, hand-written sidebar, install tabs, Geist theme.
- Animated "How it works" diagram and a getting-started video, recorded from HTML sources with
  headless Chrome and ffmpeg (`make media`, sources in `docs/assets/media/`).
- The binary prints a start banner with the UI URL, the ingest endpoints and the MCP command.
- `examples/machine`: a dependency-free machine agent (CPU, memory, load, network, disk, busiest
  processes over OTLP) and a project folder (`CLAUDE.md`, `.mcp.json`) that lets an assistant such
  as tiny-assistant or Claude Code start collecting, build a dashboard and act on the machine;
  `make machine`, `make machine-agent`, `make machine-hog`; a recorded demo video.
- "What makes it different" docs page and landing section; materialized panels documented as
  designed, not built.
- Materialized panels memory bounds: `materialize.max_panels`, `max_points_per_panel` (oldest steps are
  dropped; a panel too wide for 240 steps is not stored) and `max_slice_points` (the background slice
  shrinks for wide panels); gauges `tiny_eye_materialized_{stored_points,panels,tick_milliseconds}` and
  `..._trimmed_points_total`; the benchmark reports RSS over 240 more ticks.
- Materialized panels explainer: a worked example with ASCII drawings at the top of the docs page, and an
  animation (`docs/assets/materialized-panels.{gif,mp4,png}`, `make media`) on the landing page, README and
  "Why tiny-eye".
- Materialized dashboard panels, phase 1 of spec section 9.1: `materialize: true` on a panel
  keeps its computed points in a node-local DuckDB file (`materialize.path`, `resolution`,
  `settle`), extends them in a background task, recomputes stored steps when late samples
  arrive, and answers reads from stored points plus computed head, tail and the point at `to`,
  equal to the raw query at the same timestamps. No backfill. Responses carry `materialized`;
  `make bench` measures reads, range, coverage, ad-hoc queries, background cost and storage;
  the UI marks such panels "stored" and offers "Store points" when adding a panel.
- The default step is the smallest round step giving at most ~300 points (1h → 15s, 6h → 2m,
  12h → 3m, 24h → 5m, 7d → 1h).
- OTLP data points with the `NoRecordedValue` flag are stored as staleness markers, so a series
  that ends (a process exits) stops instead of repeating its last value for 5 minutes.
- Fleet: nodes that share `storage.url` find each other there and answer queries over all their
  data. Each node publishes a gzipped copy of its catalog (`fleet.publish_interval`) with a manifest
  `_fleet/<node.id>.json`, and attaches the other nodes' copies read-only (`fleet.refresh_interval`);
  a node alone answers as before. Another node's rows show up to about 2 minutes after they are
  acknowledged. Self-metrics `tiny_eye_fleet_nodes`, `tiny_eye_fleet_oldest_copy_age_seconds`,
  `tiny_eye_fleet_publishes_total` and `tiny_eye_fleet_errors_total` (failed publishes, failed
  refreshes and rejected manifests and copies). When two processes use the same `node.id`, the
  newest wins and the older stops publishing until the newer one's manifest has not been written
  for `coordinator.snapshot_grace`. Other nodes' manifests and catalog copies are untrusted: a
  manifest larger than 64 KiB is not read, and a copy is rejected when its key or data files are
  outside the publisher's prefix, it is larger than `fleet.max_copy_bytes` (new, default 256MB)
  compressed or decompressed, it is not a gzipped DuckLake catalog, its metadata database holds
  anything but DuckLake's own tables, it defines views or macros, or its tables differ from this
  build's schema; a rejected copy is not downloaded again for the same snapshot. Manifests
  (format 2) carry the publisher's `snapshot_grace_ms`; readers use the smaller grace, take the
  earlier of the manifest's time and the bucket's as the publish time, and drop a copy that is too
  old even when a refresh fails. A refresh gives up what it has not done within
  `fleet.refresh_interval`, and a manifest that cannot be fetched keeps its node's fresh copy.
  Materialized panels recompute stored steps when rows from another node become visible late,
  counting only the files added since the copy they already include (kept in
  `materialize.path`), so restarts of a node with the same catalog recompute nothing old. A
  `postgres:` catalog is refused with non-zero fleet intervals; `_fleet` is not allowed as
  `node.id` or `node.region`.
  Guide: "Run a fleet" (`docs/examples/run-a-fleet.md`).

### Changed

- Breaking: every node writes its objects under `<node.region>/<node.id>/` of `storage.url`
  (`local/node-0/` by default) and envelopes as `commits/<epoch>-<seq>.json` there. Data written by
  earlier builds is not read and not migrated: the node refuses to start on a catalog written by
  an earlier build (or after `node.region`/`node.id` changed) and says so. Start with an empty data
  directory, and keep the old build to read the old data. `node.id` and `node.region` must be
  letters, digits, `.`, `_` or `-`; an empty value falls back to the default.
- `freshness_s` in query responses is the age of the oldest other-node catalog copy the query read
  (still 0 on a node alone; it was always 0).
- Retention expires a catalog snapshot once a newer one has existed for `coordinator.snapshot_grace`
  (default 10m, was a fixed 10 minutes after the snapshot was created), validated to cover
  `fleet.publish_interval + 3 × fleet.refresh_interval + query.timeout`. `0s` is accepted only for
  the two fleet intervals.
- `--role` other than `all` says that only `--role all` exists and that several nodes run as a
  fleet.

### Fixed

- Flush retry order, connection pool leak on abandoned queries, auth and validation issues found in review.
- `exists` conditions bound the attribute key twice and failed.
- Dashboard vars are applied to spans/logs conditions of list kinds; `search.order` and whole-trace
  condition operators are validated; spans/logs problem paths name `spans[j]` / `logs[j]`.
- Saving a dashboard keeps the file's leading comments; `run-panel.sh` applies dashboard vars.
- Retention keeps series rows a day longer than points; waiting for a query connection counts
  against the query timeout.
- UI: adding a panel while editing the layout keeps the layout edits; the Add panel drawer works
  when opened before the catalog loads; resize drags read the current layout; stale validation
  responses in Explore are ignored.
- Demo shop exports metrics every 10 s (it fell back to the SDK's 60 s default).

[Unreleased]: https://github.com/tinnguyenhuuletrong/tiny-eye/commits/main
