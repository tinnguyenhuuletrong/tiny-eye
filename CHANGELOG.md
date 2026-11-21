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
- Web UI (Preact + Vite) and a UI prototype with a panel drawer, drag-and-drop and resizable
  dashboard layout.
- Generated JSON Schemas for TQS, dashboards and config, a formula grammar and MCP tool docs
  (`make schemas`, kept in sync by a test).
- Evidence: TQS conformance against Prometheus 3.16 point by point with a mutation check
  (`make conformance`), end-to-end comparison of the whole binary against Prometheus (`make e2e`),
  storage proof of concept (`make poc`), benchmarks on local disk and MinIO (`make bench`, `make bench-s3`).
- Demo: a Node.js shop instrumented with OpenTelemetry, k6 load, a dashboard, ad-hoc queries and
  MCP setup for Claude Code / Claude Desktop (`make demo`, `make demo-app`, `make demo-load`).
- Documentation site (Docusaurus, `website/`) published to GitHub Pages, and standard project files
  (license, contributing guide, security policy, code of conduct, issue and PR templates, Dependabot).

### Fixed

- Flush retry order, connection pool leak on abandoned queries, auth and validation issues found in review.

[Unreleased]: https://github.com/OWNER/tiny-eye/commits/main
