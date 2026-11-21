#!/usr/bin/env bash
# Re-runnable end to end: wipe the stack, start it, run every experiment once.
# Experiments assume an empty lake (E2 writes, E3 counts exact rows), so `reset` is not optional.
set -euo pipefail
cd "$(dirname "$0")/.."
scripts/stack.sh reset
eval "$(scripts/stack.sh env)"
cargo run --release --quiet -- lake
