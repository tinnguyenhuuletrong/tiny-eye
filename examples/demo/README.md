# Demo: a shop, a load test, and everything in tiny-eye

A small Bun and TypeScript shop sends **traces, metrics and logs** to tiny-eye with the standard OpenTelemetry
SDK. k6 generates traffic. You then read it through dashboards, ad-hoc TQS queries, or an AI agent
over MCP.

```
k6 ──HTTP──► shop-api :3000 ──HTTP──► payments :3001        (Bun + OpenTelemetry JS SDK)
                 │                         │
                 └──── OTLP/HTTP protobuf ─┴──► tiny-eye :7070 ──► ./data/demo (Parquet + DuckLake)
                                                    ▲
                     dashboards (YAML) · curl/TQS · MCP (Claude Code, Claude Desktop, …)
```

The shop has realistic trouble built in: search is sometimes slow, about 6 % of card
authorizations hit a 1.5–2.7 s provider stall (half of those time out → `502` on `/checkout`), 4 %
are declined, and a few product lookups 404. Every error writes a log line that carries its trace id.

## Run it

Prerequisites: Rust (for tiny-eye), [Bun](https://bun.sh) 1.x, Docker (for k6), `jq` for the helper script.

```bash
make tools                 # once: pinned DuckDB extensions (+ Prometheus for the test suite)
make demo                  # terminal 1: tiny-eye on :7070 with examples/demo/tiny-eye.yaml
make demo-app              # terminal 2: shop-api :3000 + payments :3001
make demo-load             # terminal 3: k6 for 3 minutes (DURATION=10m make demo-load for longer)
```

Data appears within seconds: rows are queryable as soon as they are acknowledged (the hot tail),
and are flushed to Parquet every 15 s in this config. Check the engine's own counters with
`curl -s localhost:7070/metrics | grep ingested`.

## Look at the data

### Dashboards (views stored in git)

`examples/demo/dashboards/shop.yaml` defines the **Shop overview**: request rate, 5xx error rate,
p95 latency and orders/min as stats; requests by route, latency p50/p95/p99, failures by status,
card-authorization p95 by outcome and error spans per service as charts; a routes table; recent
warnings/errors, error patterns and slow checkouts.

```bash
./examples/demo/run-panel.sh shop            # list the panels
./examples/demo/run-panel.sh shop latency    # run one: last point of p50/p95/p99
curl -s localhost:7070/api/v1/dashboards/shop | jq '.panels[].query.title'
```

Each panel is a plain TQS document, so what the dashboard shows is exactly what
`POST /api/v1/query` returns for it.

**Add or change a view.** Each panel is `query` (TQS: what to fetch) plus `ui` (`display`, `unit`,
and its place: `w` of 12 columns, `h` rows; panels flow in list order). Either edit the YAML (VS
Code autocompletes and validates it through the `# yaml-language-server: $schema=…` header, from
`schema/generated/dashboard.schema.json`) and restart, or send the whole new state with
`PUT /api/v1/dashboards/<id>` (admin scope), which validates it and rewrites the file. The file
is the source of truth either way, so commit it. To try a panel before saving it:

```bash
curl -s localhost:7070/api/v1/validate -d @my-panel.json | jq   # every problem, with its JSON path
curl -s localhost:7070/api/v1/query    -d @my-panel.json | jq
```

### Pick a view by hand (ad-hoc queries)

Start from what exists, then write the query:

```bash
curl -s localhost:7070/api/v1/catalog | jq '.metrics[] | {name, label_keys}'   # metrics + labels
curl -s localhost:7070/api/v1/catalog | jq '.services'                          # services, span names
```

Ready-made queries in `examples/demo/queries/`:

| File | What it answers |
| --- | --- |
| `checkout-p99-by-status.json` | p99 latency of `/checkout`, split by status code |
| `failed-checkout-traces.json` | traces where the payments service recorded an error, slowest first |
| `payment-timeouts.json` | the matching log lines, each with its trace id |

```bash
curl -s localhost:7070/api/v1/query -d @examples/demo/queries/failed-checkout-traces.json | jq '.rows[0]'

# then open one trace as a waterfall
T=$(curl -s localhost:7070/api/v1/query -d @examples/demo/queries/failed-checkout-traces.json | jq -r '.rows[0].trace_id')
curl -s localhost:7070/api/v1/query -d "{\"tqs\":\"3\",\"id\":\"w\",\"kind\":\"trace\",\"trace_id\":\"$T\",\"time\":{\"from\":\"-30m\"}}" \
  | jq -r '.rows[] | "\("  " * (.depth|tonumber))\(.service_name) \(.span_name) \(.duration_ms)ms status=\(.status_code)"'
```

```
shop-api POST /checkout 2719.820ms status=2
  shop-api SELECT cart_items 18.198ms status=0
  shop-api POST 2700.278ms status=2
    payments POST /authorize 2699.162ms status=2
      payments provider.charge 2696.777ms status=0
```

The language is documented in the spec (§8) and as a JSON Schema in `schema/generated/tqs.schema.json`;
`api/examples/` has one example per query kind.

### Web UI

There is no web UI in the engine yet. A clickable prototype of the planned one (dashboards, a
point-and-click view builder that writes TQS, trace waterfall, log patterns) was shared separately
for review; it uses sample data.

## Connect an AI agent (MCP)

`tiny-eye mcp` is a stdio MCP server that forwards to a running node. Its tools are generated from
`api/openapi.yaml` and listed in `docs/reference/mcp-tools.md`: `discover_telemetry_catalog`,
`validate_tqs`, `run_tqs`. It has read-only access.

Build the binary once with `make build`, then:

**Claude Code**

```bash
claude mcp add tiny-eye -- "$PWD/target/release/tiny-eye" mcp --url http://127.0.0.1:7070
```

**Claude Desktop, Cursor and other MCP clients** (`mcpServers` in the client's config file):

```json
{
  "mcpServers": {
    "tiny-eye": {
      "command": "/absolute/path/to/tiny-eye/target/release/tiny-eye",
      "args": ["mcp", "--url", "http://127.0.0.1:7070"]
    }
  }
}
```

With auth enabled on the node (`auth.tokens` in the config), add `--token-env TINY_EYE_QUERY_TOKEN`
and set that variable in the client's environment.

Things to ask once k6 has been running for a few minutes:

- "Which route of shop-api has the worst p99 latency right now, and why?"
- "Find the slowest failed checkout in the last 15 minutes and show me its spans."
- "What are the most common error log patterns, and which service do they come from?"
- "Compare card authorization latency for ok, declined and timeout outcomes."

The agent discovers metric and label names with `discover_telemetry_catalog`, writes TQS, and
fixes its own mistakes from the problem codes `validate_tqs` returns. Telemetry text comes back
inside a `<data>` block so it is not mistaken for instructions.

## Other senders

- **Any OpenTelemetry SDK**: set `OTEL_EXPORTER_OTLP_ENDPOINT=http://<tiny-eye>:7070` and
  `OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf` (or `http/json`). gRPC (`:4317`) is not supported yet.
- **Prometheus / Grafana Alloy / OTel Collector scraping `/metrics`**: remote-write to
  `http://<tiny-eye>:7070/api/v1/write`.
- **Loki clients**: push JSON to `/loki/api/v1/push` (protobuf push is not supported yet).

The demo exercises the OTLP path only; remote-write is covered by `make e2e`.

## Troubleshooting

| Symptom | Cause / fix |
| --- | --- |
| No logs (`ingested_rows_total{table="logs"} 0`) with recent `@opentelemetry/*` packages | The logs API now lives in `@opentelemetry/api` (`logs`, `SeverityNumber`); a separate `@opentelemetry/api-logs` package registers a provider the SDK never reads. Also pass log processors an options object: `new BatchLogRecordProcessor({ exporter })`. |
| A seconds histogram shows p95 ≈ 4.75 for everything | The SDK's default buckets (0, 5, 10, 25 …) are for milliseconds. Give `createHistogram` `advice.explicitBucketBoundaries` in seconds, as `server.ts` does. |
| HTTP metrics have no `http_route` label | `Bun.serve` is not auto-instrumented. `server.ts` records `http.server.request.duration` itself, in the `traced()` wrapper that also knows each route's template. |
| A trace stops at the shop and payments starts a new one | Bun's `fetch` is not auto-instrumented either. `server.ts` injects the trace context into the outgoing headers (`propagation.inject`) and `traced()` extracts it. |
| Panels show nothing for delta counters | tiny-eye stores cumulative temporality (the SDK default) and skips delta points. |
| `/checkout` returns 502 sometimes | Intended: the payments provider times out on purpose. |
