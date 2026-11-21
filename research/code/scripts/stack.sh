#!/usr/bin/env bash
# Local PoC stack: MinIO (S3) + a private PostgreSQL cluster (DuckLake catalog).
# Usage: scripts/stack.sh up | down | reset | env
# Nothing here touches the machine's system PostgreSQL cluster: the PoC owns its own
# data directory and port so `down`/`reset` can never destroy someone else's database.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
STATE="$ROOT/.state"
PG_BIN="${PG_BIN:-/usr/lib/postgresql/16/bin}"
PG_PORT="${PG_PORT:-55432}"
PG_DATA="$STATE/pg"
PG_SOCK="$STATE/pgsock"
MINIO_BIN="${MINIO_BIN:-$ROOT/.tools/minio}"
MINIO_DATA="$STATE/minio"
MINIO_PORT="${MINIO_PORT:-9000}"
export MINIO_ROOT_USER="${MINIO_ROOT_USER:-pocadmin}"
export MINIO_ROOT_PASSWORD="${MINIO_ROOT_PASSWORD:-pocadmin-secret}"
BUCKETS=(telemetry-us-east telemetry-eu-west)

# Commands run as the postgres OS user because initdb refuses root.
as_pg() { su postgres -s /bin/bash -c "$*"; }

pg_up() {
  mkdir -p "$PG_SOCK"; chown postgres "$PG_SOCK"
  if [[ ! -f "$PG_DATA/PG_VERSION" ]]; then
    mkdir -p "$PG_DATA"; chown postgres "$PG_DATA"
    as_pg "$PG_BIN/initdb -D '$PG_DATA' -A trust -U postgres >/dev/null"
  fi
  if ! as_pg "$PG_BIN/pg_ctl -D '$PG_DATA' status" >/dev/null 2>&1; then
    as_pg "$PG_BIN/pg_ctl -D '$PG_DATA' -l '$PG_SOCK/pg.log' -w \
      -o \"-p $PG_PORT -k '$PG_SOCK' -c listen_addresses=127.0.0.1\" start" >/dev/null
  fi
  psql -h 127.0.0.1 -p "$PG_PORT" -U postgres -tAc "select 1 from pg_database where datname='ducklake'" | grep -q 1 \
    || psql -h 127.0.0.1 -p "$PG_PORT" -U postgres -qc "create database ducklake"
  echo "postgres: 127.0.0.1:$PG_PORT db=ducklake"
}

minio_up() {
  [[ -x "$MINIO_BIN" ]] || { echo "minio binary missing at $MINIO_BIN: build it with" \
    "'GOBIN=$ROOT/.tools go install github.com/minio/minio@latest'" >&2; exit 1; }
  mkdir -p "$MINIO_DATA"
  if ! curl -sf "http://127.0.0.1:$MINIO_PORT/minio/health/live" >/dev/null; then
    nohup "$MINIO_BIN" server "$MINIO_DATA" --address "127.0.0.1:$MINIO_PORT" \
      --console-address "127.0.0.1:$((MINIO_PORT + 1))" >"$STATE/minio.log" 2>&1 &
    echo $! >"$STATE/minio.pid"
  fi
  # Converge on the health endpoint instead of sleeping a guessed amount.
  for _ in $(seq 1 60); do curl -sf "http://127.0.0.1:$MINIO_PORT/minio/health/live" >/dev/null && break; sleep 0.5; done
  curl -sf "http://127.0.0.1:$MINIO_PORT/minio/health/live" >/dev/null \
    || { echo "minio did not become healthy; see $STATE/minio.log" >&2; exit 1; }
  for b in "${BUCKETS[@]}"; do
    code=$(curl -s -o /dev/null -w '%{http_code}' --aws-sigv4 "aws:amz:us-east-1:s3" \
      --user "$MINIO_ROOT_USER:$MINIO_ROOT_PASSWORD" -X PUT "http://127.0.0.1:$MINIO_PORT/$b")
    [[ "$code" == 200 || "$code" == 409 ]] || { echo "create bucket $b failed: HTTP $code" >&2; exit 1; }
  done
  echo "minio: http://127.0.0.1:$MINIO_PORT buckets=${BUCKETS[*]}"
}

down() {
  [[ -f "$STATE/minio.pid" ]] && kill "$(cat "$STATE/minio.pid")" 2>/dev/null || true
  rm -f "$STATE/minio.pid"
  as_pg "$PG_BIN/pg_ctl -D '$PG_DATA' -m fast stop" >/dev/null 2>&1 || true
}

env_out() {
  cat <<EOF
export POC_S3_ENDPOINT=http://127.0.0.1:$MINIO_PORT
export POC_S3_KEY=$MINIO_ROOT_USER
export POC_S3_SECRET=$MINIO_ROOT_PASSWORD
export POC_PG="host=127.0.0.1 port=$PG_PORT user=postgres dbname=ducklake"
export POC_DUCKDB_EXT=$ROOT/.tools/duckdb_ext
EOF
}

case "${1:-}" in
  up) mkdir -p "$STATE"; pg_up; minio_up ;;
  down) down ;;
  reset) down; rm -rf "$STATE"; mkdir -p "$STATE"; pg_up; minio_up ;;
  env) env_out ;;
  *) echo "usage: $0 up|down|reset|env" >&2; exit 2 ;;
esac
