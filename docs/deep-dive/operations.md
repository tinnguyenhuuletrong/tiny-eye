---
title: Operations
description: Running a tiny-eye node or a fleet of nodes, choosing storage, sizing, self-monitoring, shutdown, backups, upgrades and troubleshooting.
sidebar_position: 12
---

This page is for the person who runs tiny-eye. One process runs ingest, the coordinator (checkpoint, compaction, retention) and queries. Several such nodes can share one bucket and answer queries over each other's data; see [Run a fleet](#run-a-fleet) below.

## Running a single node

```bash
make tools        # once: pinned DuckDB extensions (and Prometheus for tests) into .tools/
make build        # target/release/tiny-eye
./target/release/tiny-eye --config /etc/tiny-eye/config.yaml
```

Command line:

| Command | Does |
| :--- | :--- |
| `tiny-eye [--config <file>]` | Run the node. `--role all` is accepted; any other role is refused. |
| `tiny-eye config [--config <file>]` | Print the effective configuration (secrets redacted) and exit. |
| `tiny-eye mcp --url <node> [--token-env VAR]` | Run the stdio MCP bridge against a running node. |
| `tiny-eye schemas [--root .]` | Write the generated grammars (developers). |
| `tiny-eye --help` / `--version` | Print the usage or the version. An unknown flag or command is refused with a pointer to `--help`, so a typo never starts a node with the defaults. |

Logs go to stderr; set the level with `RUST_LOG` (default `info`). At start the node replays any WAL left from a previous run, logs `serving` with the listen address, and prints a short block that starts with `tiny-eye <version> is ready` and lists the addresses to use. The effective configuration is logged at `debug` (`RUST_LOG=debug`); `tiny-eye config` prints it without starting. On Ctrl-C or SIGTERM it logs `shutting down`, flushes and checkpoints, and ends with a `stopped:` line.

### Required files and directories

- **DuckDB extensions** `icu`, `ducklake`, `httpfs` (and `postgres_scanner` for a PostgreSQL catalog) for the bundled DuckDB version (v2.0.1). Set `duckdb.extension_dir` to a directory laid out as `<dir>/v2.0.1/<platform>/*.duckdb_extension` (`make tools` fills `.tools/duckdb_ext`). With an empty `extension_dir`, DuckDB downloads them on first start, which needs network access.
- **Writable paths:** `storage.url` (local mode), `catalog.url`, `ingest.wal_dir`, `fleet.dir`, `materialize.path`, and `dashboards.dir` if dashboards are saved from the UI.

### systemd

A sketch of a unit file (adapt paths and user):

```ini
[Unit]
Description=tiny-eye observability engine
After=network-online.target

[Service]
User=tiny-eye
WorkingDirectory=/var/lib/tiny-eye
ExecStart=/usr/local/bin/tiny-eye --config /etc/tiny-eye/config.yaml
EnvironmentFile=/etc/tiny-eye/secrets.env
KillSignal=SIGTERM
TimeoutStopSec=120
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

`secrets.env` holds the token variables named by `auth.tokens[].token_env` and, for S3, `TINY_EYE__STORAGE__S3__ACCESS_KEY_ID` and `TINY_EYE__STORAGE__S3__SECRET_ACCESS_KEY`. Give `TimeoutStopSec` enough time for the final flush (see [Shutdown](#shutdown-and-flush)).

### Containers

The repository's `Dockerfile` builds one image with the web UI embedded and the pinned DuckDB
extensions inside (`make docker`, tag `tiny-eye:dev`). The release workflow publishes it to
`ghcr.io/tinnguyenhuuletrong/tiny-eye` for amd64 and arm64 on version tags.

```bash
docker run -p 7070:7070 -v tiny-eye-data:/data tiny-eye:dev
```

What the image sets for you, through `TINY_EYE__*` environment variables:

- `server.listen: 0.0.0.0:7070`, so the port is reachable from outside the container;
- `duckdb.extension_dir: /opt/tiny-eye/duckdb_ext`, so the node needs no network at start;
- storage, catalog, WAL, materialized panels and fleet copies under the `/data` volume, and dashboards in `/etc/tiny-eye/dashboards`
  (writable by the `tinyeye` user, so saving from the UI works; mount your own directory there);
- it runs as a non-root user (uid 10001).

Override any key with another `-e TINY_EYE__SECTION__KEY=value`, or mount a config file and pass
`--config /path/file.yaml`. The node handles `SIGTERM`, so `docker stop` flushes buffered rows if
the stop timeout (default 10 s) covers the final flush; raise it with `--stop-timeout` for large
arenas. Building the image compiles DuckDB: give Docker at least 8 GB of memory, and raise
`--build-arg BUILD_JOBS=` on bigger builders.

`deploy/docker-compose.yml` only starts MinIO for S3 tests and benchmarks; it does not run tiny-eye.

## Storage: local disk or S3

| | Local disk | S3 (or MinIO) |
| :--- | :--- | :--- |
| `storage.url` | `./data/objects` or `file:///abs/dir` | `s3://bucket/prefix` |
| Use for | development, small single-machine installs | durable production storage |
| Durability of flushed data | the machine's disk | the object store |
| Commit safety | create-only writes for envelopes | conditional PUT (`If-None-Match`) so a retried flush never overwrites a committed envelope |

Objects are laid out under the node's own prefix `<node.region>/<node.id>/` (`local/node-0/` by default):

```text
<region>/<id>/<table>/tenant=<tenant>/date=<YYYY-MM-DD>/hour=<HH>/l0-<ulid>-<node>.parquet     flushed data
<region>/<id>/<table>/tenant=<tenant>/date=<YYYY-MM-DD>/hour=<HH>/l1-<ulid>-compacted.parquet  compacted data
<region>/<id>/commits/<epoch>-<seq>.json                                                        pending envelopes
<region>/<id>/_fleet/catalog.ducklake.gz                                                        published catalog copy
_fleet/<id>.json                                                                                fleet manifest
```

In both modes the **catalog** (`catalog.url`, a DuckDB file by default) and the **WAL** (`ingest.wal_dir`) are on local disk. Put them on durable storage. See [Storage format](storage-format.md) and [Write path](write-path.md).

For S3, set `storage.s3.region`, the credentials (through the env vars), and for MinIO `storage.s3.endpoint` plus `allow_http: true` if it is plain HTTP.

## Sizing hints

From one run of `make bench` and `make bench-s3` on an 8-core laptop (4 DuckDB threads, `wal` durability, 2,620 series, 6 hours at a 15 s scrape interval, 3.7 M samples):

| Measure | Result |
| :--- | :--- |
| Remote-write ingest, 4 clients | about 1.65 M samples/s, request p99 18 ms |
| Flush of the whole backfill | 0.5 s (local), 0.6 s (MinIO) |
| Checkpoint | 50 ms (local), 93 ms (MinIO) |
| 6 h panel `sum by (job) (rate(...[5m]))`, 3,410 points | about 580 ms p50 |
| 6 h histogram p99 | about 73 ms p50 |
| One series over 6 h | about 21 ms p50 |
| Trace lookup by id | about 10 to 13 ms |
| OTLP spans | about 95,000 to 117,000 spans/s, 33.5 bytes per span on L0 |
| Absent trace id after compaction, cold caches (MinIO) | 2 GETs (22 before compaction) |

:::warning Caveats
These are numbers from one run on one machine. The synthetic counters rise linearly, so the measured 0.4 bytes per sample is far below real data (the research measured about 6 bytes per sample on random walks); measure on your own exporters before sizing disk. Panel timings are with warm DuckDB caches. MinIO ran on the same host, so S3 latency was about 1 ms instead of a real region's 20 to 50 ms.
:::

Practical starting points:

- **Memory.** The hot tail holds everything since the last flush plus `hot_grace`; `ingest.arena_limit` (1 GB) caps it, and DuckDB has its own `duckdb.memory_limit` (4 GB). Budget RAM for both.
- **CPU.** Heavy range panels scale with `duckdb.threads`; `query.workers` bounds how many queries run at once.
- **Disk.** The WAL holds at most the unflushed data (bounded by `flush_bytes` / `flush_interval` and the arena). Object storage grows with retention (`15d` by default).

Full reports: [local disk](evidence/bench-local.md), [MinIO](evidence/bench-s3.md). They are refreshed every week on a GitHub-hosted runner, so their numbers can differ from this table; the run block at the top of each report says where and when it ran. More in [Performance](performance.md).

## Self-metrics

`GET /metrics` (public) serves Prometheus text format. Every metric from `crates/engine/src/metrics.rs`:

| Metric | Type | Labels | Meaning |
| :--- | :--- | :--- | :--- |
| `tiny_eye_ingested_rows_total` | counter | `table` (`metrics_series`, `metrics_points`, `spans`, `logs`) | Rows admitted. |
| `tiny_eye_rejected_rows_total` | counter | `reason` = `out_of_bounds` | Rows dropped for being older than `max_sample_age` or further ahead than `max_sample_future`. |
| `tiny_eye_rejected_rows_total` | counter | `reason` = `overload` | Rows refused with `429` because the arena was full. |
| `tiny_eye_flushes_total` | counter | | Flushes written to object storage. |
| `tiny_eye_flush_milliseconds_total` | counter | | Time spent flushing. |
| `tiny_eye_flushed_bytes_total` | counter | | Parquet bytes written by flushes. |
| `tiny_eye_checkpoints_total` | counter | | Catalog checkpoints that applied envelopes. |
| `tiny_eye_compactions_total` | counter | | L0 to L1 compactions. |
| `tiny_eye_queries_total` | counter | | TQS queries run (valid documents). |
| `tiny_eye_query_errors_total` | counter | | TQS queries that failed while running. |
| `tiny_eye_query_milliseconds_total` | counter | | Time spent running queries. |
| `tiny_eye_materialized_points_total` | counter | | Points stored for [materialized panels](materialized-panels.md). |
| `tiny_eye_materialized_reads_total` | counter | | Queries answered from materialized panel points. |
| `tiny_eye_materialized_trimmed_points_total` | counter | | Stored points dropped because a panel passed `materialize.max_points_per_panel`. |
| `tiny_eye_materialized_stored_points` | gauge | | Points now stored for materialized panels. |
| `tiny_eye_materialized_panels` | gauge | | Panels currently materialized. |
| `tiny_eye_materialized_tick_milliseconds` | gauge | | Duration of the last materialization tick. |
| `tiny_eye_hot_bytes` | gauge | | Bytes buffered in the ingest arena and hot tail. |
| `tiny_eye_fleet_publishes_total` | counter | | Catalog copies published for the other nodes of the fleet. |
| `tiny_eye_fleet_errors_total` | counter | | Failed fleet publishes, failed refreshes, and other nodes' catalog copies that were rejected or could not be attached. |
| `tiny_eye_fleet_nodes` | gauge | | Other nodes sharing storage whose data this node's queries read. |
| `tiny_eye_fleet_oldest_copy_age_seconds` | gauge | | Age of the oldest other-node catalog copy queries read (0 when alone). |

Useful derived signals: average flush time is `rate(flush_milliseconds_total) / rate(flushes_total)`; average query time is `rate(query_milliseconds_total) / rate(queries_total)`; `tiny_eye_hot_bytes` close to `ingest.arena_limit` means `429`s are near.

Not exported yet (spec §13): commit lag, compaction backlog, bytes scanned, cache hits, per-query latency histograms.

### Self dashboard

`dashboards/self.yaml` (`tiny-eye-self`) charts ingested rows per second by table and the hot tail size. It reads `tiny_eye_*` metrics with label `job="tiny-eye"`, so scrape the node's `/metrics` into the node itself with any Prometheus agent, for example:

```yaml
scrape_configs:
  - job_name: tiny-eye
    static_configs: [{ targets: ["127.0.0.1:7070"] }]
remote_write:
  - url: http://127.0.0.1:7070/api/v1/write
```

## Run a fleet

Nodes that share one `storage.url` find each other there and answer queries over all their data ([Run a fleet](../examples/run-a-fleet.md) has the setup and the limits). For the operator:

- **Give every node its own `node.id`** across the whole fleet, and set `node.region` (neither may be `_fleet`). Keep `storage.url` (and the S3 settings) identical on every node, as an absolute path or `s3://` URL, and `catalog.url`, `ingest.wal_dir` and `materialize.path` local to each. Use a DuckDB file catalog: a `postgres:` catalog is refused unless both fleet intervals are `0s`.
- **Use the same timing on every node:** `fleet.publish_interval`, `fleet.refresh_interval`, `fleet.max_copy_bytes`, `query.timeout` and `coordinator.snapshot_grace`. A node's grace protects the copies other nodes read. Keep the clocks synced with NTP: a reader compares a manifest's publish time with its own clock.
- **Freshness.** Another node's rows show up here at most `ingest.flush_interval + coordinator.checkpoint_interval + fleet.publish_interval + fleet.refresh_interval` after they were acknowledged (up to about 2 minutes with the defaults). `freshness_s` in responses and `tiny_eye_fleet_oldest_copy_age_seconds` give the age of the oldest copy read.
- **Watch** `tiny_eye_fleet_nodes` (a drop means a node stopped publishing, is older than the grace, or its copy was rejected) and `tiny_eye_fleet_errors_total` (failed publishes, failed refreshes and rejected copies; the log says which).
- **Stopping a node.** Its last copy keeps answering on the others until its manifest is older than `coordinator.snapshot_grace` (10 minutes), then it drops out with a warning on each other node. Its manifest stays in `_fleet/`; delete `_fleet/<id>.json` to retire a node for good. No node runs retention on a retired node's prefix, so expire its objects with a bucket lifecycle rule on `<region>/<id>/`, or delete the prefix.
- **Requests to the bucket.** Each publish is a GET of the node's own manifest (the duplicate-id check) and one or two PUTs (the copy only when the catalog changed). Each refresh is a LIST of `_fleet/`, one GET per manifest, and one GET per changed copy.
- **Credentials.** Every node reads every other node's prefix, so it needs read access to the whole `storage.url`, write and delete access to its own prefix, and write access to `_fleet/`. A laptop node needs the same. Other nodes' catalog copies are checked before they are attached ([Security](security.md#nodes-of-a-fleet)).

## Shutdown and flush

On `SIGINT` or `SIGTERM` the node:

1. stops accepting connections and lets in-flight requests finish;
2. stops the background loops (flusher, checkpointer, compaction, retention);
3. flushes everything buffered to object storage and applies it to the catalog in one final checkpoint;
4. exits.

With `durability: wal`, a crash or `kill -9` loses no acknowledged row: WAL segments that never reached object storage are replayed into the hot tail at the next start (proved by `wal_replay_after_crash_loses_nothing` in `crates/engine/tests/node.rs`). With `durability: memory`, a crash loses the unflushed batch. A failed flush is retried in order; while it keeps failing, ingest continues until the arena limit answers `429`.

## Backups

The state of a node is:

| Part | Where | Back up |
| :--- | :--- | :--- |
| Data files and pending envelopes | `storage.url` | Object-store versioning or replication (S3), or file-level backup (local disk) |
| Catalog | `catalog.url` (DuckDB file) | Copy the file; it is small. The published copy in `<region>/<id>/_fleet/` is at most `fleet.publish_interval` old, but it is not a backup: it is replaced at every publish |
| WAL | `ingest.wal_dir` | Only matters for a crash; not needed for a clean backup after shutdown |
| Dashboards | `dashboards.dir` | In git (commit saves from the UI) |
| Configuration | your config file | In git (tokens are env vars) |

The catalog references data files by URL, so the catalog and the objects must match. The safest backup is taken after a clean shutdown: stop the node (which flushes and checkpoints), copy the catalog file, snapshot the object store, start the node. A catalog copy taken while running may be mid-transaction. Retention deletes files older than `coordinator.retention` and expires a catalog snapshot once a newer one has existed for `coordinator.snapshot_grace` (10 minutes), so a restored catalog must not be older than the objects it points to.

## Upgrades

- **DuckDB is pinned.** The `duckdb` crate is pinned to an exact version (DuckDB v2.0.1) and the extensions must match that version and platform. A DuckDB upgrade moves the crate, the extensions (`make tools` downloads the new version into a new versioned directory) and the DuckLake catalog format together; test it on a copy of the catalog first.
- **Object layout.** Objects live under `<node.region>/<node.id>/` of `storage.url` since the fleet was added. Data written by earlier builds (objects at the root, envelopes under `commits/<region>/`) is not read and not migrated, and a newer build refuses to start on a catalog written that way (see [Troubleshooting](#troubleshooting)). Start with an empty data directory, and keep the old build if you still need to read the old data. Changing `node.region` or `node.id` of an existing node has the same effect.
- **Envelope format.** Envelopes carry `format: 1`. A binary refuses an envelope with another format and names both, so a newer envelope left in `commits/` stops checkpointing instead of being misread.
- **Fleet manifest format.** Manifests carry `format: 2`. A node skips a manifest of another format with a warning, so during a rolling upgrade that changes it, nodes on different versions do not read each other.
- **Fleet and table schema.** A node leaves out another node's catalog copy whose tables or columns differ from its own schema (counted in `tiny_eye_fleet_errors_total`). During a rolling upgrade that changes `schema/tables.yaml`, each node answers over its own data and the nodes on the same build until every node is upgraded.
- **Parquet schema version.** Data files carry `schema_version` (from `schema/tables.yaml`, currently 1) in their metadata. It is written but not yet checked on read.
- **Log patterns.** `pattern_version` (currently 1) is stored next to every `pattern_hash`; changing the masking rules changes hashes, so patterns before and after an upgrade do not group together.
- **Procedure.** Stop the old binary (it flushes and checkpoints), back up the catalog, start the new binary, check `/healthz` and the start logs.

## Troubleshooting

| Symptom | Likely cause | Fix |
| :--- | :--- | :--- |
| Start fails with `LOAD ducklake from ... (run make tools)` | Extensions missing for this DuckDB version or platform | Run `make tools`, or point `duckdb.extension_dir` at the right directory |
| Start fails with `auth token env X is not set` | A `token_env` variable is missing | Export it (for example via `EnvironmentFile`) |
| Start fails with `unknown key` or `invalid configuration` | Typo in the config file or a `TINY_EYE__` variable | Fix the key; check with `tiny-eye config` |
| Start fails with `<file> does not validate` | A dashboard YAML is invalid | Fix the listed problems; validate with the dashboard schema in your editor |
| Start fails with `binding 127.0.0.1:7070` | Port in use | Change `server.listen` |
| Start fails with `another tiny-eye (PID n) already uses <catalog>` | A second node on the same machine uses the same file catalog | Stop the first one, or give the second its own `catalog.url` and `ingest.wal_dir` (and `server.listen`); to share data, run them as a [fleet](../examples/run-a-fleet.md) |
| `tiny-eye mcp` tools answer `cannot reach tiny-eye at <url>` | The node is not running, or `--url` points elsewhere | Start the node (`make run`) or fix `--url` |
| Ingest answers `429` | Arena full: flushes are slow or failing | Check logs for `flush failed`, object-store reachability, `tiny_eye_hot_bytes`; raise `arena_limit` only with RAM to match |
| Samples missing, `rejected_rows_total{reason="out_of_bounds"}` rising | Clock skew or backfill older than `max_sample_age` | Fix clocks; raise `ingest.max_sample_age` for backfills |
| `401 missing or unknown bearer token` | No token, or a token not in `auth.tokens` | Check `Authorization: Bearer` and the token's environment variable |
| `403 this token lacks the ... scope` | A known token without the operation's scope | Use a token with that scope or `admin`; see the scope table in [Security](security.md) |
| Query answers `500 ... exceeded the 30000 ms timeout` | Heavy panel over a long range | Raise `step`, narrow the range or the selection, or raise `query.timeout` |
| Query answers `400` | Invalid TQS | Read `problems`; each has a code and path ([TQS reference](tqs-reference.md#problem-codes)) |
| Logs show `checkpoint failed; will retry` with `has format N` | Envelope written by a different binary version | Run the matching binary, or move the envelope aside after checking it |
| `kind: trace` lookups slow | No `time` range given | Pass `time` so fewer files are probed |
| Start fails with `the catalog ... was written for objects under ..., but this node writes under ...` | The catalog was written by a build before the fleet layout, or `node.region`/`node.id` changed since | Move `./data` (catalog, WAL and objects) aside and start empty, or set `node.region` and `node.id` back. Keep the old build to read the old data |
| Start fails with `coordinator.snapshot_grace (...) must be at least ...` | The grace is shorter than `fleet.publish_interval + 3 × fleet.refresh_interval + query.timeout` | Raise `coordinator.snapshot_grace`, or lower the intervals, on every node |
| Start fails with `node.id "..." names an object storage prefix` | `node.id` or `node.region` has a character other than letters, digits, `.`, `_`, `-` | Rename it |
| Logs show `node.id "..." is used by another node too` | Two running nodes share one `node.id`; the newer process won and this one stopped publishing | Give each node its own id, then restart the one you renamed |
| `tiny_eye_fleet_errors_total` rises and logs say a copy was rejected | Another node's copy failed a check (key or files outside its prefix, too large, views or macros, or another table schema) | Check that node's build and config; during a rolling upgrade, finish it |
| Logs show `skipping a fleet manifest older than coordinator.snapshot_grace` | That node stopped publishing more than the grace ago | Start it again, or delete its `_fleet/<id>.json` if it is retired |
| Another node's data is missing or late | Not yet published and refreshed, or that node has `fleet.publish_interval: 0s` | Check `freshness_s`, `tiny_eye_fleet_nodes` and that node's `tiny_eye_fleet_publishes_total` |
| `/` shows "The web UI is not built into this binary" | Binary built without `ui/dist` | Build the UI and the binary with `make build` (or `make ui`, then `cargo build --release`) |
| Dashboard save fails with `io` | `dashboards.dir` not writable | Fix permissions, or edit the YAML in git |

See also [Configuration](configuration.md) and [Catalog and maintenance](catalog-and-maintenance.md).
