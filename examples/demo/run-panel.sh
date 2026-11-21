#!/usr/bin/env bash
# Run one panel of a dashboard and print its series/rows: ./run-panel.sh shop latency
# Needs curl and jq. TINY_EYE_URL defaults to http://127.0.0.1:7070.
set -euo pipefail
url="${TINY_EYE_URL:-http://127.0.0.1:7070}"
dash="${1:?usage: run-panel.sh <dashboard id> [panel id]}"
if [[ -z "${2:-}" ]]; then
  curl -sf "$url/api/v1/dashboards/$dash" | jq -r '.panels[].query | "\(.id)\t\(.kind)\t\(.title // "")"'
  exit 0
fi
curl -sf "$url/api/v1/dashboards/$dash" \
  | jq --arg id "$2" '.panels[] | select(.query.id == $id) | .query' \
  | curl -sf "$url/api/v1/query" -H 'Content-Type: application/json' -d @- \
  | jq '{elapsed_ms, series: [.series[] | {name: (if .key == "" then .output else .key end), last: .points[-1]}], rows: .rows[:5]}'
