---
title: HTTP API
description: Every tiny-eye HTTP endpoint with its operationId, scope, request, response and status codes, generated from api/openapi.yaml.
sidebar_position: 1
---

The HTTP API is defined in `api/openapi.yaml` (OpenAPI 3.1). The router, the response types, the auth scope of each route and the MCP tools are generated from that file, so this page and the server cannot disagree on routes or scopes. A running node serves the full spec at **`GET /openapi.yaml`**.

Base URL: `http://<host>:7070` by default (`server.listen`). Scopes apply when `auth.tokens` is not empty; send `Authorization: Bearer <token>`. See [Security](../deep-dive/security.md).

## Endpoints

| Method | Path | operationId | Scope | Purpose |
| :--- | :--- | :--- | :--- | :--- |
| GET | `/healthz` | `healthz` | public | Liveness probe |
| GET | `/metrics` | `self_metrics` | public | Engine self-metrics, Prometheus text format |
| GET | `/openapi.yaml` | `openapi_spec` | public | The API spec |
| POST | `/api/v1/query` | `run_query` | query | Run a TQS 3 document (MCP tool `run_tqs`) |
| POST | `/api/v1/validate` | `validate_query` | query | Validate a TQS 3 document without running it (MCP tool `validate_tqs`) |
| GET | `/api/v1/catalog` | `discover_catalog` | query | Bounded discovery of metrics, labels, services and span names (MCP tool `discover_telemetry_catalog`) |
| GET | `/api/v1/dashboards` | `list_dashboards` | query | List dashboards |
| GET | `/api/v1/dashboards/{id}` | `get_dashboard` | query | One dashboard with its panels |
| PUT | `/api/v1/dashboards/{id}` | `save_dashboard` | admin | Save a dashboard |
| POST | `/api/v1/write` | `prom_remote_write` | ingest | Prometheus remote-write 1.0 |
| POST | `/v1/metrics` | `otlp_metrics` | ingest | OTLP/HTTP metrics |
| POST | `/v1/traces` | `otlp_traces` | ingest | OTLP/HTTP traces |
| POST | `/v1/logs` | `otlp_logs` | ingest | OTLP/HTTP logs |
| POST | `/loki/api/v1/push` | `loki_push` | ingest | Loki push (JSON) |

HEAD on a GET route needs the GET scope. Any other method or path needs `admin` when auth is on; with enough scope, an unknown path under `/api/`, `/v1/` or `/loki/` returns `404` (`no such API route; see GET /openapi.yaml`). Every other GET path serves the web UI (public).

## Status codes

| Code | When |
| :--- | :--- |
| 200 | Success (query, validate, catalog, dashboards, OTLP ingest) |
| 204 | Remote-write and Loki push accepted and durable |
| 400 | Invalid TQS document or dashboard (JSON `ProblemResponse`), or an undecodable ingest payload (plain text) |
| 401 | Missing or insufficient bearer token (plain text) |
| 404 | Unknown dashboard, or unknown API route |
| 413 | Request body over 64 MB |
| 429 | Ingest arena full; retry later (plain text) |
| 500 | Query failure or timeout, or an internal error (plain text with the error message) |

"Durable" depends on `ingest.durability`: with `wal` the rows are fsynced to the local WAL before the response; with `memory` they are in RAM. See [Write path](../deep-dive/write-path.md).

## Health, metrics and spec

### `GET /healthz`

Returns `200` with `ok` (text/plain).

```bash
curl -s http://127.0.0.1:7070/healthz
```

### `GET /metrics`

Returns `200` with the engine's counters and gauges in Prometheus text format (`text/plain; version=0.0.4`). Metric list: [Operations](../deep-dive/operations.md#self-metrics).

### `GET /openapi.yaml`

Returns `200` with this API's OpenAPI document (`application/yaml`), the same file the server was built from.

```bash
curl -s http://127.0.0.1:7070/openapi.yaml -o openapi.yaml
```

## Query

### `POST /api/v1/query`

Request body: a TQS 3 document (`TqsDocument`, `application/json`). Language: [TQS reference](../deep-dive/tqs-reference.md).

Response `200`: `QueryResponse`.

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `tqs` | `"3"` | |
| `id` | string | The document `id` |
| `snapshot_id` | integer | This node's own DuckLake snapshot the query read |
| `freshness_s` | number | Age in seconds of the oldest catalog copy of another node sharing storage that the query read (`0` when the node is alone; its own rows are visible once acknowledged) |
| `partial_regions` | string[] | Always empty |
| `series` | `Series[]` | `timeseries` / `table` kinds: `{output, key, labels, points}`, `points` are `[epoch_ms, value]` |
| `rows` | object[] | List kinds: each row maps column to text |
| `rows_total` | integer | Points (series kinds) or rows (list kinds) |
| `truncated` | boolean | List kinds: the row count reached `limit` |
| `elapsed_ms` | number | Server-side time |
| `materialized` | object, optional | Present when stored points of a [materialized panel](../deep-dive/materialized-panels.md) answered: `panel`, `resolution_ms`, `stored_points`, `computed_steps`. The series are then on the step-aligned grid. |

Response `400`: `ProblemResponse`, `{"valid": false, "problems": [{"code", "path", "message"}]}` with every problem. Response `500`: plain text, for example a timeout.

```bash
curl -s http://127.0.0.1:7070/api/v1/query \
  -H "Authorization: Bearer $TINY_EYE_QUERY_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"tqs":"3","id":"rps","kind":"timeseries","time":{"from":"-1h","step":"1m"},
       "queries":[{"name":"r","metric":"http_requests_total",
                   "fn":{"op":"rate","window":"5m"},"agg":{"op":"sum","by":["job"]}}]}'
```

```json
{ "tqs": "3", "id": "rps", "snapshot_id": 12, "freshness_s": 0.0, "partial_regions": [],
  "series": [ { "output": "r", "key": "job=api", "labels": { "job": "api" },
                "points": [[1762743600000, 4.25], [1762743660000, 4.31]] } ],
  "rows": [], "rows_total": 2, "truncated": false, "elapsed_ms": 21.7 }
```

A list kind returns `rows` instead:

```bash
curl -s http://127.0.0.1:7070/api/v1/query -H 'Content-Type: application/json' \
  -d '{"tqs":"3","id":"errors","kind":"log_search","time":{"from":"-15m"},
       "search":{"logs":[{"field":"severity_number","op":">=","value":17}]},"limit":20}'
```

### `POST /api/v1/validate`

Request body: a TQS 3 document. Always returns `200` with `ProblemResponse`: `{"valid": true, "problems": []}` or `{"valid": false, "problems": [...]}`.

```bash
curl -s http://127.0.0.1:7070/api/v1/validate -H 'Content-Type: application/json' \
  -d '{"tqs":"3","id":"x","kind":"timeseries","time":{"from":"-1h"},
       "queries":[{"name":"a","metric":"m","fn":{"op":"rate"}}]}'
```

```json
{ "valid": false,
  "problems": [ { "code": "window", "path": "queries[0].fn.window", "message": "Rate needs a window" } ] }
```

### `GET /api/v1/catalog`

No request body. Response `200`: `CatalogResponse`.

| Field | Meaning |
| :--- | :--- |
| `metrics[]` | `{name, type, series, label_keys, label_values}` for up to 500 metric names (alphabetical). `type` is `counter`, `gauge`, `histogram`, `summary` or `unknown` (remote-write metrics are `unknown`). `label_values` holds up to 50 distinct values per label key, a sample for pickers. |
| `services[]` | `{name, span_names, log_lines}` for services seen in spans or logs in the last 24 h; up to 50 span names each. |
| `truncated` | `true` when there were more than 500 metric names. |

```bash
curl -s http://127.0.0.1:7070/api/v1/catalog -H "Authorization: Bearer $TINY_EYE_QUERY_TOKEN"
```

## Dashboards

Model and save semantics: [Dashboards and the web UI](../deep-dive/dashboards-and-ui.md).

### `GET /api/v1/dashboards`

Response `200`: array of `DashboardSummary` `{id, title, panels}` (`panels` is the count).

```bash
curl -s http://127.0.0.1:7070/api/v1/dashboards
```

### `GET /api/v1/dashboards/{id}`

Response `200`: `Dashboard` `{id, title, vars, panels: [{query, ui}]}`, exactly as stored (dashboard `vars` are not merged into the queries). `404` with `unknown dashboard` when no dashboard has that id.

### `PUT /api/v1/dashboards/{id}`

Scope `admin`. Request body: the whole `Dashboard`. The body `id` must equal the URL id. The engine validates it like a file loaded at start (shape, id, `ui` values, every panel's TQS), writes it atomically to the file that holds this id (or `<id>.yaml`) in `dashboards.dir`, and serves it from then on.

Response `200`: the saved `Dashboard`. Response `400`: `ProblemResponse` with paths such as `panels[1].ui.w`; nothing is written.

```bash
curl -s -X PUT http://127.0.0.1:7070/api/v1/dashboards/shop \
  -H "Authorization: Bearer $TINY_EYE_ADMIN_TOKEN" \
  -H 'Content-Type: application/json' \
  --data @shop.json
```

## Ingest

The OTLP and Loki endpoints accept `Content-Encoding: gzip` or `snappy` (and `identity`); remote-write bodies are always snappy-compressed. Decompressed bodies are limited to 256 MB. Rows outside `[now - max_sample_age, now + max_sample_future]` are dropped without failing the request (counted in `tiny_eye_rejected_rows_total{reason="out_of_bounds"}`). Attribute values with secret-looking keys are redacted. More: [Sending data](../examples/sending-data.md).

### `POST /api/v1/write`

Prometheus remote-write 1.0: a snappy block-compressed protobuf `WriteRequest` (`application/x-protobuf`). `__name__` is the metric; empty labels are dropped; staleness markers are kept. Responses: `204`, `400`, `429`.

```yaml
# prometheus.yml
remote_write:
  - url: http://127.0.0.1:7070/api/v1/write
    authorization: { credentials_file: /etc/prometheus/tiny-eye-token }
```

### `POST /v1/metrics`, `POST /v1/traces`, `POST /v1/logs`

OTLP/HTTP, protobuf (`application/x-protobuf`) or JSON (`application/json`). Responses: `200` (empty body), `400`, `429`.

Metrics are converted to Prometheus-shaped series: `job` and `instance` from `service.name` (prefixed by `service.namespace/` when set) and `service.instance.id`; monotonic cumulative sums get a `_total` suffix; histograms become `_bucket{le}`, `_sum` and `_count`; summaries become `{quantile}`, `_sum` and `_count`. Delta temporality and exponential histograms are skipped.

```yaml
# OpenTelemetry Collector
exporters:
  otlphttp:
    endpoint: http://127.0.0.1:7070
    headers: { Authorization: "Bearer ${env:TINY_EYE_INGEST_TOKEN}" }
```

```bash
curl -s http://127.0.0.1:7070/v1/logs -H 'Content-Type: application/json' -d '{
  "resourceLogs": [{ "resource": { "attributes": [{ "key": "service.name", "value": { "stringValue": "api" } }] },
    "scopeLogs": [{ "logRecords": [{ "timeUnixNano": "'$(date +%s)'000000000", "severityNumber": 17,
      "body": { "stringValue": "payment timeout after 30s" } }] }] }] }'
```

### `POST /loki/api/v1/push`

Loki push, JSON only (`LokiPush`): `{"streams": [{"stream": {labels}, "values": [["<unix ns>", "<line>"], ...]}]}`. A value may carry a third element, a structured-metadata object; its `trace_id` links the line to a trace. The service name comes from the first of the labels `service_name`, `service`, `app`, `job`; the level from `level`, `detected_level` or `severity`. Responses: `204`, `400`, `429`.

```bash
curl -s http://127.0.0.1:7070/loki/api/v1/push -H 'Content-Type: application/json' \
  -d '{"streams":[{"stream":{"service_name":"web","level":"warn"},
       "values":[["'$(date +%s)'000000000","slow request 120ms"]]}]}'
```

## Schemas

The response and request schemas are in `components.schemas` of `api/openapi.yaml`: `QueryResponse`, `Series`, `Point`, `ProblemResponse`, `Problem`, `CatalogResponse`, `CatalogMetric`, `CatalogService`, `DashboardSummary`, `Dashboard`, `Panel`, `PanelUi`, `LokiPush`, and the TQS schemas (`TqsDocument` and its parts). Standalone JSON Schemas for TQS documents and dashboards are in `schema/generated/` (see [Editor grammars](editor-grammars.md)). MCP tools: [MCP tools](mcp-tools.md).
