---
title: Concepts
description: The few ideas behind tiny-eye - signals and tables, TQS documents, panels and dashboards, freshness, and where things live in the repository.
sidebar_position: 3
---

This page explains the vocabulary used everywhere else in the docs. Read it once; the rest of
the documentation assumes it.

## Signals and tables

tiny-eye stores three signals in four tables. `schema/tables.yaml` defines every column; the
Arrow schemas, Parquet writer, DuckLake DDL and in-memory DDL are generated from it.

| Signal | Table | One row is | Sorted by |
| --- | --- | --- | --- |
| Metrics | `metrics_series` | a series: metric name, type, unit, labels | `series_id` |
| Metrics | `metrics_points` | a sample: series id, timestamp (ms), value, staleness flag | `series_id`, `timestamp_ms` |
| Traces | `spans` | a span: trace id, span id, parent, start, duration, service, name, kind, status, attributes | `trace_id`, `start_time_ns` |
| Logs | `logs` | a log record: timestamp, service, severity, body, pattern hash, trace id, attributes | `service_name`, `severity_number`, `timestamp_ns` |

Metrics follow the Prometheus data model: a metric name plus a set of labels identifies a series.
OTLP metrics are translated into that shape at ingest (see
[Sending data](../examples/sending-data.md)). Spans and logs keep their OpenTelemetry fields and
two attribute maps each (resource attributes and span or log attributes).

## TQS documents

A TQS 3 document is a JSON object with a closed schema: an unknown field is an error, not a
silently ignored option. The same document is used by the HTTP API, dashboards, the web UI and
the MCP tools.

```json
{
  "tqs": "3",
  "id": "error-pct",
  "kind": "timeseries",
  "time": { "from": "-1h", "step": "1m" },
  "queries": [
    { "name": "e", "metric": "http_requests_total",
      "match": [{ "label": "code", "op": "=~", "value": "5.." }],
      "fn": { "op": "rate", "window": "5m" }, "agg": { "op": "sum", "by": ["job"] }, "fill": "zero" },
    { "name": "a", "metric": "http_requests_total",
      "fn": { "op": "rate", "window": "5m" }, "agg": { "op": "sum", "by": ["job"] } }
  ],
  "formulas": [{ "name": "pct", "expr": "100 * e / a" }]
}
```

The parts:

- **`tqs`** is always `"3"`. **`id`** names the document (`^[a-z][a-z0-9_-]{0,31}$`).
- **`kind`** decides the shape of the answer:
  - `timeseries`: one value per series per step.
  - `table`: one value per series at `time.to` (an instant query).
  - `trace_search`, `trace`, `log_search`, `log_patterns`: lists of rows.
- **`time`** has `from`, optional `to` (default `"now"`) and optional `step`. Points are epoch
  milliseconds, `"now"`, or `"-<duration>"` such as `"-2h"`. Only `trace` may omit `time`, but you
  should still pass it.
- **`queries`** (1 to 10) each read exactly one source: `metric`, `spans` or `logs`. A metric
  query selects series (`match`), applies a range function (`fn`), aggregates (`agg`), and can
  shift (`offset`), filter (`having`), relabel (`derive`), aggregate again (`then`) or fill gaps
  (`fill`). A spans or logs query filters rows and computes a `measure` per step.
- **`formulas`** combine query results with arithmetic and a small function set (`abs`, `round`,
  `least`, `greatest`, `clamp`, `coalesce`). Division by zero gives no value instead of infinity.
- **`top`**, **`vars`**, **`search`**, **`trace_id`** and **`limit`** complete the schema.

`POST /api/v1/validate` returns every problem at once, each with a code and a JSON path, so a
person or an agent can fix them all in one round trip. The full language is in the
[TQS reference](../deep-dive/tqs-reference.md); worked recipes are in the
[Query cookbook](../examples/query-cookbook.md).

## Panels and dashboards

A **panel** is a TQS document plus presentation:

- `query`: what to fetch (a TQS document).
- `ui`: how to draw it and where. `display` is one of `timeseries`, `stat`, `table`, `bar`,
  `traces`, `logs`; `unit` is a label such as `req/s`; `w` is the width in columns of a 12-column
  grid; `h` is the height in rows (1 to 8).

A **dashboard** is a YAML file with an `id`, a `title`, optional `vars` (defaults for every panel's
variables) and a list of panels. Panels flow over the grid in list order. Dashboards live in git
under the directory set by `dashboards.dir` (`./dashboards` by default), are validated at start,
and are served by `GET /api/v1/dashboards/{id}`. `PUT /api/v1/dashboards/{id}` validates a new
state and writes it back to the file, which stays the source of truth. See
[Dashboards as code](../examples/dashboards-as-code.md).

## Freshness: hot tail and lake

Data lives in two places, and every query reads both:

- **Hot tail**: rows that arrived recently, held in an in-memory DuckDB. A request is
  acknowledged once its rows are in the write-ahead log and in the hot tail, so they are
  queryable immediately.
- **Lake**: Parquet files recorded in the DuckLake catalog.

A query reads the lake plus the hot rows the lake does not have yet (above a per-node
watermark). Each row is counted once. Committed rows stay in RAM for `coordinator.hot_grace`
after the catalog has them, then are released.

## Flush and checkpoint

A **flush** happens every `ingest.flush_interval` (60 s by default) or when the open batch
reaches `ingest.flush_bytes` (64 MB). It writes one sorted, ZSTD-compressed Parquet file per
table and hour, with a bloom filter on `trace_id`, then commits an **envelope** that lists the
files, using a create-only PUT. A **checkpoint** runs every `coordinator.checkpoint_interval`
(5 s): it applies all pending envelopes to DuckLake in one transaction. It is idempotent: an
envelope already applied is skipped. Hourly **compaction** merges an hour's files once that hour
is `compaction_delay` old, and **retention** (15 days by default) deletes old files through
DuckLake. Details: [Write path](../deep-dive/write-path.md) and
[Catalog and maintenance](../deep-dive/catalog-and-maintenance.md).

## One binary, several roles

The same `tiny-eye` binary has a few subcommands:

| Command | What it does |
| --- | --- |
| `tiny-eye [--config file.yaml]` | Runs a node. Single-node mode (`--role all`) runs ingest, coordinator and query in one process. |
| `tiny-eye mcp --url http://127.0.0.1:7070 [--token-env VAR]` | Runs the MCP stdio bridge for AI agents against a running node. |
| `tiny-eye config [--config file.yaml]` | Prints the effective configuration, secrets redacted. |
| `tiny-eye schemas [--root .]` | Regenerates editor and agent grammars (`make schemas`). |

Separate ingest, coordinator and query roles for a cluster are planned but not built; any
`--role` other than `all` is refused at start.

## Where things live in the repository

| Path | Contents |
| --- | --- |
| `api/` | `openapi.yaml`, the source of truth for the HTTP API, auth scopes and MCP tools; `examples/` has one TQS document per query kind. |
| `config/` | `default.yaml` (every default, compiled into the binary) and `dev.yaml` (used by `make run`). |
| `crates/tqs` | The TQS model, validator, formula parser and SQL compiler, plus the conformance suite against Prometheus. |
| `crates/engine` | Storage: WAL, hot tail, flush, checkpoint, compaction, retention, queries, dashboards, config. |
| `crates/server` | The binary: HTTP handlers, protocol receivers, auth, MCP bridge, embedded web UI, benchmark. |
| `dashboards/` | Dashboards as YAML (`self.yaml` monitors tiny-eye itself). |
| `deploy/` | `docker-compose.yml` for a local MinIO. |
| `docs/` | This documentation. `deep-dive/evidence/` holds generated test and benchmark reports. |
| `examples/` | `demo/`: an instrumented Node.js shop, a k6 load script, a dashboard and queries. |
| `research/` | The specification (`research/spec/`) and the research code and results it is based on. |
| `schema/` | `tables.yaml` (source of truth for stored tables) and `generated/` (JSON Schemas for TQS, dashboards and config, MCP tool list, formula grammar). |
| `ui/` | The web UI (Preact + Vite, embedded in the binary) and `prototype/`, the clickable design prototype on sample data. |
| `website/` | The Docusaurus site that publishes `docs/`. |
