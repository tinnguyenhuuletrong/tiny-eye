# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html). There are no releases yet.

## [Unreleased]

### Added

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
- Demo: a Node.js shop instrumented with OpenTelemetry, k6 load, a dashboard, ad-hoc queries and
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
