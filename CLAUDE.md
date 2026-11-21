# tiny-eye — working agreement

Blob-first observability engine (SPEC: `doc/spec/Observability Engine Spec.md`, research
evidence: `doc/research/`). Metrics, logs and traces → sorted ZSTD Parquet in object storage →
DuckLake catalog → queried with TQS 3 (closed JSON schema compiled to DuckDB SQL).

## Principles

1. **Keep it simple.** The smallest design that meets the spec and its evidence. No layer,
   trait, crate or config knob without a second user. Three crates: `tqs`, `engine`, `server`.
2. **One source of truth, generated everywhere else.** Never hand-write what a definition
   already says:
   | Definition | Generates |
   | --- | --- |
   | `api/openapi.yaml` | axum router (operationId → `handlers::<id>`), response types, MCP tool list |
   | `schema/tables.yaml` | Arrow schemas, typed row builders, DuckLake + hot-tail DDL |
   | `config/default.yaml` | every default; the binary embeds it, user YAML + `TINY_EYE__*` env override |
   | `dashboards/*.yaml` | dashboards and panels (TQS documents in git) |
   Change the definition, rebuild; the compiler points at whatever must follow.
3. **Configuration-driven.** Behaviour (sizes, intervals, durability, retention, storage,
   auth) comes from config, not constants in code.
4. **Proven, not claimed.** Every semantic rule has a test that can fail: TQS against
   Prometheus point by point (`make conformance`, with a mutation check), storage
   assumptions in `crates/engine/tests/poc.rs`, the whole binary in `make e2e`, numbers in
   `make bench`. Results land in `doc/results/`.

## Commands

`make help` lists everything. Usual loop: `make fmt lint test`. Before a PR: `make check`.
`make tools` downloads pinned Prometheus + DuckDB extensions into `.tools/` (D12).

## Conventions

- Rust 2024, `cargo fmt` (max width 140), clippy `-D warnings`, `unsafe` forbidden.
- Every query string is a bound parameter; only validated identifiers/numbers are formatted.
- Errors: `anyhow` with context at boundaries; HTTP maps to problem codes (SPEC §8.12).
- Don't edit generated code (`target/**/out/*.rs`); edit the YAML.
