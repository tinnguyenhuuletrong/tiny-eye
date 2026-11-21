---
title: Installation
description: Build tiny-eye from source, run it with a config file, and store data on local disk or S3.
sidebar_position: 2
---

tiny-eye is built from source with Cargo. Every developer task is a `make` target; `make help`
lists them. This page covers the build, the configuration layers, where data lives, and how to
run against S3.

## Prerequisites

| Tool | Why |
| --- | --- |
| Rust stable (1.90 or newer) | Builds the binary. `rust-toolchain.toml` selects the stable channel. |
| A C/C++ compiler | The `duckdb` crate compiles DuckDB from source. The first build takes several minutes. |
| `curl`, `tar`, `gunzip` | `make tools` downloads pinned binaries. |
| Node.js 20+ and npm | Builds the web UI, which `make build` and `make run` embed. Also needed for the demo shop and the docs site. Not needed if you build with `cargo` directly. |
| Docker (optional) | Only for MinIO (`make s3-up`) and the demo load test (`make demo-load`). |

## Build from source

```bash
git clone https://github.com/OWNER/tiny-eye.git
cd tiny-eye
make tools    # once: pinned DuckDB extensions + Prometheus into .tools/
make build    # web UI (ui/dist), then the release binary: target/release/tiny-eye
```

Replace `OWNER` with the GitHub owner of the repository you clone from.

### What `make tools` downloads

`make tools` fills `.tools/` (git-ignored) with pinned versions, so builds and tests do not
depend on what is installed on your machine:

- **DuckDB 2.0.1 extensions** for your platform, in `.tools/duckdb_ext/v2.0.1/<platform>/`:
  `ducklake` (the catalog), `httpfs` (S3 reads), `icu` and `postgres_scanner`. DuckLake needs `icu`
  because its snapshot functions use `TIMESTAMPTZ` arithmetic. `postgres_scanner` is there for a
  Postgres-hosted catalog.
- **Prometheus 3.16.0** (`prometheus` and `promtool`). The engine does not need it at run time.
  The conformance suite (`make conformance`) and the end-to-end test (`make e2e`) compare
  tiny-eye's answers with real Prometheus.

`make run`, `make test` and the other targets that need these files depend on `make tools`, so
the download happens once.

### Release build

`make build` first runs `make ui` (`npm ci` and `npm run build` in `ui/`, output in `ui/dist`),
then `cargo build --release --bin tiny-eye`. The build embeds `ui/dist` into the binary, so the
binary is all you need to deploy. DuckDB must be a release build in every profile (its debug
build aborts when release-built extensions free memory), which `Cargo.toml` already arranges.

Without Node.js, build the binary alone:

```bash
cargo build --release --bin tiny-eye
```

If `ui/dist` does not exist, that binary serves a short page at `/` that points to the API
instead of the web UI. Everything else works the same.

## Run

```bash
make run                                        # config/dev.yaml, listens on 127.0.0.1:7070
./target/release/tiny-eye --config my.yaml      # your own config file
./target/release/tiny-eye config --config my.yaml   # print the effective config, secrets redacted
```

`make run` runs `make tools` and `make build` if needed, then starts the binary with
`config/dev.yaml`: data under `./data`, a 10 s flush interval, a 2 s checkpoint interval, and
the DuckDB extensions from `.tools/duckdb_ext`. Relative paths in a config file are
relative to the working directory, so start the binary from the repository root or use absolute
paths.

:::note
The default `duckdb.extension_dir` is empty. In that case DuckDB installs extensions from its
extension host into `~/.duckdb/extensions` on first start, which needs network access. Point
`duckdb.extension_dir` at a copy of `.tools/duckdb_ext` to run offline.
:::

## Configuration layers

Configuration is built in three layers, each overriding the one before:

1. `config/default.yaml`, compiled into the binary. It holds every default, with comments.
2. The file passed with `--config <path>`, merged over the defaults.
3. Environment variables named `TINY_EYE__SECTION__KEY`.

After the `TINY_EYE__` prefix, double underscores separate the levels, and the key names are
matched in lower case:

```bash
TINY_EYE__INGEST__FLUSH_INTERVAL=5s          # ingest.flush_interval: 5s
TINY_EYE__SERVER__LISTEN=0.0.0.0:7070        # server.listen
TINY_EYE__STORAGE__S3__ENDPOINT=http://127.0.0.1:19000   # storage.s3.endpoint
```

Values are parsed as YAML, so numbers and booleans keep their type. An unknown key stops the
start with an error naming it. The effective configuration is logged at start with secrets
redacted. Every key is described in [Configuration](../deep-dive/configuration.md).

:::tip
Editors autocomplete and validate config files through the
`# yaml-language-server: $schema=../schema/generated/config.schema.json` header at the top of
`config/*.yaml`.
:::

## Data directory layout

With the default config, everything lives under `./data` (git-ignored):

```text
data/
  wal/                      write-ahead log segments (durability: wal)
  catalog.ducklake          DuckLake catalog (a DuckDB file) and its .wal
  objects/                  the object store root (storage.url)
    metrics_series/tenant=default/date=2026-11-21/hour=00/l0-<ulid>-node-0.parquet
    metrics_points/tenant=default/date=2026-11-21/hour=15/l0-<ulid>-node-0.parquet
    spans/...               same layout
    logs/...                same layout
    commits/local/...json   flush envelopes waiting for the checkpointer (deleted once applied)
    lake/                   DuckLake's own data path
```

Files are partitioned by table, tenant, date and hour. `l0-` files come from flushes; hourly
compaction replaces them with `l1-…-compacted.parquet` files. The full key layout is in
[Storage format](../deep-dive/storage-format.md).

`make clean` removes build output, `./data` and `.state`.

## Store data on S3 or MinIO

Set `storage.url` to `s3://bucket/prefix`. The catalog (`catalog.url`) stays a local file in
single-node mode.

For a local MinIO, `make s3-up` starts one on `127.0.0.1:19000` (console on `:19001`) with user
`tinyeye`, password `tinyeye-secret` and a bucket named `tiny-eye`:

```bash
make s3-up
TINY_EYE__STORAGE__URL=s3://tiny-eye/dev \
TINY_EYE__STORAGE__S3__ENDPOINT=http://127.0.0.1:19000 \
TINY_EYE__STORAGE__S3__ALLOW_HTTP=true \
TINY_EYE__STORAGE__S3__ACCESS_KEY_ID=tinyeye \
TINY_EYE__STORAGE__S3__SECRET_ACCESS_KEY=tinyeye-secret \
make run
```

For AWS S3, leave `storage.s3.endpoint` empty, set `storage.s3.region`, and pass the access key
through `TINY_EYE__STORAGE__S3__ACCESS_KEY_ID` and `TINY_EYE__STORAGE__S3__SECRET_ACCESS_KEY`.
Keep credentials out of config files. `make s3-down` stops MinIO and drops its volume.

:::info
tiny-eye writes flush envelopes with a create-only PUT (`If-None-Match`), so a retried flush can
never overwrite a commit. Your S3-compatible store must support conditional writes. MinIO does,
and `make bench-s3` measures the S3 path on it.
:::

## Stop cleanly

Stop the server with Ctrl-C (SIGINT) or SIGTERM. It stops accepting requests, flushes the rows
still buffered in memory to Parquet, and runs a final checkpoint into DuckLake before it exits.

If the process dies without that flush (a crash, `kill -9`), acknowledged rows are not lost with
the default `ingest.durability: wal`: the write-ahead log is replayed on the next start. With
`durability: memory`, rows not yet flushed are lost.
