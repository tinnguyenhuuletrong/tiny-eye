# research/

Rust research code behind [../SPECIFICATION.md](../SPECIFICATION.md). It proves or refutes design
claims. It is not the product, though `src/tqs/` is meant to seed the product's `crates/tqs`.

## Commands

| Command | What it runs | Writes |
| :--- | :--- | :--- |
| `scripts/run-all.sh` | everything below from a clean stack | all of `results/` |
| `cargo run --release -- lake` | storage experiments E1–E9 (needs the stack) | `results/poc-results.md` |
| `cargo run --release -- tqs-lake` | TQS queries on DuckLake tables in MinIO vs in memory (needs the stack) | `results/tqs-lake.md` |
| `cargo run --release -- tqs` | TQS coverage vs Prometheus + Rust oracles + validation + mutation check (no stack needed) | `results/tqs-coverage.md` |
| `scripts/dashboard_coverage.py <stag-dashboards.csv>` | construct coverage of the real Margince board (margince-qc's record) | `results/dashboard-coverage.md` |
| `scripts/stack.sh up\|down\|reset\|env` | MinIO on `127.0.0.1:9000` + a private PostgreSQL on port 55432 under `.state/` | — |

## One-time setup

Needs Rust 1.85+, Go 1.24+, PostgreSQL 16 binaries (`/usr/lib/postgresql/16/bin`, or set `PG_BIN`)
and Python 3.

```bash
# MinIO, Prometheus and promtool into .tools/
GOBIN=$PWD/.tools go install github.com/minio/minio@latest
GOBIN=$PWD/.tools go install github.com/prometheus/prometheus/cmd/prometheus@latest \
                              github.com/prometheus/prometheus/cmd/promtool@latest

# DuckDB 2.0.0 extensions (PyPI wheels; extensions.duckdb.org works too where reachable)
pip download --no-deps --only-binary=:all: --platform manylinux2014_x86_64 -d /tmp/wheels \
  duckdb-extension-ducklake==2.0.0 duckdb-extension-httpfs==2.0.0 duckdb-extension-postgres-scanner==2.0.0
mkdir -p .tools/duckdb_ext/v2.0.0/linux_amd64
for w in /tmp/wheels/*.whl; do unzip -o -q "$w" -d /tmp/wx; done
cp /tmp/wx/*/extensions/v2.0.0/*.duckdb_extension .tools/duckdb_ext/v2.0.0/linux_amd64/
```

`scripts/stack.sh` never touches a system PostgreSQL cluster: it owns `.state/` and port 55432.

## Layout

| Path | Contents |
| :--- | :--- |
| `src/write.rs` | Ingest write path: series identity, Arrow batches, Parquet settings, envelopes |
| `src/lake.rs` | Object stores per region, DuckDB connections, DuckLake DDL |
| `src/exp.rs` | Storage experiments E1–E9, checkpointer, compaction swap |
| `src/synth.rs` | Synthetic traces and counters for E1–E9 |
| `src/tqs/mod.rs` | TQS 3 model and validator (problem codes) |
| `src/tqs/formula.rs` | Formula grammar → AST → SQL |
| `src/tqs/compile.rs` | TQS → DuckDB SQL; mutation switches used only by the suite |
| `src/fixtures.rs` | Margince-shaped metrics, board metrics, spans, logs; OpenMetrics export; log masking |
| `src/promref.rs` | Prometheus harness: promtool import, server, query API |
| `src/suite.rs` | M/D/R/S/L/T/V/A cases, mutation check, DuckLake equality run |
