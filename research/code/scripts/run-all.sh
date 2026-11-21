#!/usr/bin/env bash
# Every piece of research evidence, from a clean stack, in one run:
#   1. lake      — storage experiments E1–E9 on MinIO + DuckLake (results/poc-results.md)
#   2. tqs-lake  — query-schema cases on DuckLake tables in MinIO (results/tqs-lake.md)
#   3. tqs       — query-schema coverage vs Prometheus + mutation check (results/tqs-coverage.md)
#   4. dashboard — construct coverage of the real Margince board (results/dashboard-coverage.md)
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release --quiet
scripts/stack.sh reset
eval "$(scripts/stack.sh env)"
status=0
./target/release/blobobs-poc lake || status=1
./target/release/blobobs-poc tqs-lake || status=1
./target/release/blobobs-poc tqs || status=1
# Optional: construct coverage of the real Margince board, when margince-qc is checked out next to us.
board="${MARGINCE_QC:-../margince-qc}/shared/stag-dashboards.csv"
if [[ -f "$board" ]]; then python3 scripts/dashboard_coverage.py "$board" > results/dashboard-coverage.md; else echo "skip dashboard coverage: $board not found"; fi
scripts/stack.sh down
exit $status
