---
title: Dashboards as code
description: Write dashboards as YAML in git, with editor autocompletion, validation at load and saving from the UI.
sidebar_position: 5
---

Dashboards in tiny-eye are YAML files in git. The engine loads them at start, serves them over
the API, and writes them back when the UI saves a new layout. The file is always the source of
truth.

## The format

A dashboard has an `id`, a `title`, optional `vars` and a list of `panels`. Each panel is a TQS
document (`query`) plus presentation (`ui`):

```yaml
id: checkout                 # ^[a-z0-9][a-z0-9_-]{0,63}$, unique; also the file name on save
title: Checkout service
vars:                        # defaults for every panel's "$name" variables
  service: checkout
panels:
  - ui: { display: stat, unit: req/s, w: 3, h: 2 }
    query:                   # a complete TQS 3 document
      tqs: "3"
      id: requests
      title: Requests per second
      kind: table
      time: { from: "-5m" }
      queries:
        - name: r
          metric: http_server_request_duration_count
          match: [{ label: job, op: "=", value: "$service" }]
          fn: { op: rate, window: 2m }
          agg: { op: sum }
          fill: zero
```

The `ui` block:

| Field | Values | Default |
| --- | --- | --- |
| `display` | `timeseries`, `stat`, `table`, `bar`, `traces`, `logs` | `timeseries` for timeseries queries, `table` otherwise |
| `unit` | free text shown next to values, such as `req/s`, `s`, `%`, `bytes` | none |
| `w` | width in columns of a 12-column grid, 1 to 12 | 3 for `stat`, 6 otherwise |
| `h` | height in rows, 1 to 8 | 2 for `stat`, 4 otherwise |

There are no x/y coordinates. Panels flow over the grid in list order: a panel goes to the right
of the previous one if it fits in the remaining columns, otherwise to the next row. To move a
panel, move it in the list.

## Keep heavy panels fast: `materialize`

A panel over a long range re-reads every raw sample on each refresh. Add `materialize: true`
next to `ui` and `query` and tiny-eye keeps the panel's computed points, extending them as data
arrives; a read then computes only what is not stored, with the same result:

```yaml
  - ui: { display: timeseries, unit: req/s, w: 6, h: 4 }
    materialize: true
    query: { tqs: "3", id: rps, kind: timeseries, time: { from: "-6h" },
             queries: [{ name: r, metric: http_requests_total, fn: { op: rate, window: 5m }, agg: { op: sum, by: [route] } }] }
```

It applies to timeseries metric queries whose points depend only on their own timestamp;
validation tells you when a panel does not qualify. The UI marks materialized panels
**stored**. How it works and what it costs: [Materialized panels](../deep-dive/materialized-panels.md). New to the idea? Start with its [60-second explainer](../deep-dive/materialized-panels.md#the-idea-in-60-seconds).

## Editor autocompletion

The JSON Schema for dashboards is generated from the API definition into
`schema/generated/dashboard.schema.json`. Put this header on the first line of the file, with a
path relative to the file:

```yaml
# yaml-language-server: $schema=../schema/generated/dashboard.schema.json
```

VS Code (with the Red Hat YAML extension) and other editors that use `yaml-language-server` then
autocomplete every field, including the TQS query inside each panel, and underline mistakes as
you type. The repository's `.vscode/settings.json` also maps `dashboards/*.yaml` to that schema.

## Variables

Dashboard `vars` are defaults. When a panel is validated or run, each dashboard variable is added
to the panel query's `vars` unless the query defines the same name itself. They are never written
into the stored queries.

Use a variable as `"$name"` anywhere TQS accepts a value: matcher values, condition values and
windows. Values are bound as parameters, never pasted into SQL. Two built-ins are always
available as windows: `$__range` (`to - from`) and `$__interval` (the step).

## Validation at load

At start, tiny-eye reads every `.yaml` and `.yml` file in `dashboards.dir` (default
`./dashboards`; a missing directory means no dashboards). Each dashboard is validated like an API
request: the `id`, a non-empty `title`, the `ui` values, and every panel's TQS document with the
dashboard's variables applied.

A dashboard that does not validate stops the start, with the file name and every problem and
its path (for example `panels[2].query.queries[0].fn.window`). Two files with the same `id` also
stop the start. A broken dashboard is never served silently.

To check a panel before you add it, validate its query on a running node:

```bash
curl -s localhost:7070/api/v1/validate -d @my-panel.json | jq   # every problem, with its JSON path
curl -s localhost:7070/api/v1/query    -d @my-panel.json | jq   # run it
```

Files are read once at start. After you edit a file by hand, restart tiny-eye.

## Saving from the UI

The web UI edits the layout (drag to move, drag the corner to resize) and adds
panels. When you save, it sends the whole new state of the dashboard to
`PUT /api/v1/dashboards/{id}`. You can do the same by hand:

```bash
curl -s localhost:7070/api/v1/dashboards/checkout > checkout.json
# edit checkout.json: reorder panels, change ui.w / ui.h, add a panel
curl -s -X PUT localhost:7070/api/v1/dashboards/checkout \
  -H 'Content-Type: application/json' --data-binary @checkout.json | jq
```

The engine then:

1. Checks that the body's `id` matches the URL.
2. Validates the dashboard exactly as it validates a file at start. On any problem it answers
   `400` with every problem and its path, and writes nothing.
3. Writes the YAML to the file that already holds this `id`, or to `<id>.yaml` in
   `dashboards.dir`. The write is atomic (temporary file, then rename).
4. Serves the new version immediately.

With auth enabled, `PUT` needs a token with the `admin` scope, because it writes a file on the
server. Reads (`GET /api/v1/dashboards`, `GET /api/v1/dashboards/{id}`) need `query`.

:::warning
A save rewrites the file from the dashboard's data. The comment block at the top of the file,
including the `# yaml-language-server` line, is kept. Comments elsewhere in the file and your
formatting (inline `{ ... }` maps, blank lines between panels) are not. A file created by a save
gets a short generated header without a schema line; add one if you want completion.
:::

## Commit the file

A save changes a file in the dashboards directory on the server. Review it with `git diff` and
commit it like any other code. If the server's directory is not your working copy, copy the file
back, or keep the dashboards directory in a checkout that you commit from.

## A complete example

A small dashboard for one service: two stats, a latency chart, error spans, and recent error
logs. It assumes the metric names produced by an OpenTelemetry HTTP server instrumentation
(see [Sending data](sending-data.md) for how OTLP names are translated).

```yaml
# yaml-language-server: $schema=../schema/generated/dashboard.schema.json
id: checkout
title: Checkout service
vars:
  service: checkout
panels:
  - ui: { display: stat, unit: req/s, w: 3, h: 2 }
    query:
      tqs: "3"
      id: requests
      title: Requests per second
      kind: table
      time: { from: "-5m" }
      queries:
        - name: r
          metric: http_server_request_duration_count
          match: [{ label: job, op: "=", value: "$service" }]
          fn: { op: rate, window: 2m }
          agg: { op: sum }
          fill: zero

  - ui: { display: stat, unit: "%", w: 3, h: 2 }
    query:
      tqs: "3"
      id: error-rate
      title: Error rate (5xx)
      kind: table
      time: { from: "-5m" }
      queries:
        - name: e
          metric: http_server_request_duration_count
          match: [{ label: job, op: "=", value: "$service" }, { label: http_response_status_code, op: "=~", value: "5.." }]
          fn: { op: rate, window: 2m }
          agg: { op: sum }
          fill: zero
          hidden: true
        - name: a
          metric: http_server_request_duration_count
          match: [{ label: job, op: "=", value: "$service" }]
          fn: { op: rate, window: 2m }
          agg: { op: sum }
          hidden: true
      formulas:
        - { name: pct, expr: "100 * e / greatest(a, 0.001)" }

  - ui: { display: timeseries, unit: s, w: 6, h: 4 }
    query:
      tqs: "3"
      id: latency
      title: Latency p50 / p95 by route
      kind: timeseries
      time: { from: "-1h", step: "30s" }
      queries:
        - { name: p50, metric: http_server_request_duration, match: [{ label: job, op: "=", value: "$service" }], fn: { op: histogram_quantile, q: 0.5, window: 2m }, agg: { op: sum, by: [http_route] } }
        - { name: p95, metric: http_server_request_duration, match: [{ label: job, op: "=", value: "$service" }], fn: { op: histogram_quantile, q: 0.95, window: 2m }, agg: { op: sum, by: [http_route] } }

  - ui: { display: timeseries, unit: spans/s, w: 6, h: 4 }
    query:
      tqs: "3"
      id: span-errors
      title: Error spans per second
      kind: timeseries
      time: { from: "-1h", step: "30s" }
      queries:
        - name: e
          spans: [{ field: service_name, op: "=", value: "$service" }, { field: status_code, op: "=", value: 2 }]
          measure: { op: rate }
          agg: { by: [span_name] }
          fill: zero

  - ui: { display: logs, w: 6, h: 4 }
    query:
      tqs: "3"
      id: recent-errors
      title: Recent errors
      kind: log_search
      time: { from: "-15m" }
      search:
        logs: [{ field: service_name, op: "=", value: "checkout" }, { field: severity_number, op: ">=", value: 17 }]
        order: desc
      limit: 50
```

The first two panels share a row (3 + 3 columns), and the latency chart fills the remaining 6.
The next two panels form a second row. Save it as `dashboards/checkout.yaml`, restart tiny-eye,
and check it:

```bash
curl -s localhost:7070/api/v1/dashboards | jq
curl -s localhost:7070/api/v1/dashboards/checkout | jq '.panels[].query.title'
```

For a larger example, see `examples/demo/dashboards/shop.yaml` and the
[demo shop](demo-shop.md). The UI side is described in
[Dashboards and UI](../deep-dive/dashboards-and-ui.md).
