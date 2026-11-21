---
title: Query cookbook
description: Twenty TQS recipes for metrics, spans, logs and traces, each with its PromQL equivalent and a note on semantics.
sidebar_position: 2
---

Each recipe below answers one question with a complete TQS 3 document. Every document passes
`POST /api/v1/validate`. Where PromQL can express the same thing, the PromQL is shown; for
metrics, TQS returns the same points as Prometheus.

Run any recipe with curl:

```bash
curl -s localhost:7070/api/v1/query -d @recipe.json | jq
```

Metric names such as `http_requests_total` are examples: list your own with
`curl -s localhost:7070/api/v1/catalog | jq '.metrics[] | {name, label_keys}'`. The language
itself is specified in the [TQS reference](../deep-dive/tqs-reference.md).

## Metrics

### 1. Request rate by label

How many requests per second does each job serve in production?

```json
{ "tqs": "3", "id": "rps-by-job", "kind": "timeseries", "time": { "from": "-1h", "step": "1m" },
  "queries": [
    { "name": "rps", "metric": "http_requests_total",
      "match": [{ "label": "env", "op": "=", "value": "prod" }],
      "fn": { "op": "rate", "window": "5m" },
      "agg": { "op": "sum", "by": ["job"] } } ] }
```

PromQL: `sum by (job) (rate(http_requests_total{env="prod"}[5m]))`

`rate` corrects counter resets and extrapolates to the window edges exactly like Prometheus. The
window is left-open: the point at `t` reads samples in `(t - 5m, t]`. A function that needs two
samples gives no point when the window holds fewer, so a window shorter than the scrape interval
returns nothing, as in Prometheus.

### 2. Total over the dashboard range

How many requests per status code over the whole selected range?

```json
{ "tqs": "3", "id": "requests-in-range", "kind": "table", "time": { "from": "-24h" },
  "queries": [
    { "name": "n", "metric": "http_requests_total",
      "fn": { "op": "increase", "window": "$__range" },
      "agg": { "op": "sum", "by": ["code"] } } ] }
```

PromQL (Grafana): `sum by (code) (increase(http_requests_total[$__range]))`

`kind: table` evaluates once, at `time.to`. `$__range` is `to - from` (here 24 h), so the window
covers the whole range. Like Prometheus, `increase` is extrapolated, so the result is not
always a whole number.

### 3. Error ratio with a formula

What percentage of each job's requests fail with a 5xx, showing 0 for jobs without errors?

```json
{ "tqs": "3", "id": "error-pct", "kind": "timeseries", "time": { "from": "-6h", "step": "1m" },
  "queries": [
    { "name": "e", "metric": "http_requests_total",
      "match": [{ "label": "code", "op": "=~", "value": "5.." }],
      "fn": { "op": "rate", "window": "5m" }, "agg": { "op": "sum", "by": ["job"] },
      "fill": "zero", "hidden": true },
    { "name": "a", "metric": "http_requests_total",
      "fn": { "op": "rate", "window": "5m" }, "agg": { "op": "sum", "by": ["job"] },
      "hidden": true } ],
  "formulas": [{ "name": "pct", "expr": "100 * e / a" }] }
```

PromQL: `100 * sum by (job) (rate(http_requests_total{code=~"5.."}[5m])) / sum by (job) (rate(http_requests_total[5m]))`

The two queries are matched on step and key (`job`). `hidden: true` computes a query without
returning it, so only `pct` comes back. Regexes are RE2 and fully anchored, as in PromQL.
`fill: "zero"` on a grouped query makes a missing key count as 0 in a formula, so a job without
5xx shows 0 % where PromQL shows no value. Division by zero gives no point, not `NaN` or `Inf`.

### 4. p95 latency from a histogram

What is the 95th percentile request latency per route?

```json
{ "tqs": "3", "id": "p95-by-route", "kind": "timeseries", "time": { "from": "-3h", "step": "1m" },
  "queries": [
    { "name": "p95", "metric": "http_request_duration_seconds",
      "fn": { "op": "histogram_quantile", "q": 0.95, "window": "5m" },
      "agg": { "op": "sum", "by": ["route"] } } ] }
```

PromQL: `histogram_quantile(0.95, sum by (route, le) (rate(http_request_duration_seconds_bucket[5m])))`

Name the base metric; TQS reads `<metric>_bucket` and adds `le` to the grouping itself. It
takes the rate per bucket, sums per group and `le`, fixes non-monotonic buckets, and
interpolates linearly. A quantile that falls in the `+Inf` bucket returns the highest finite
bound, as in Prometheus.

### 5. Top 5 series

Which 5 routes are busiest at each step?

```json
{ "tqs": "3", "id": "top-routes", "kind": "timeseries", "time": { "from": "-1h", "step": "1m" },
  "queries": [
    { "name": "rps", "metric": "http_requests_total",
      "fn": { "op": "rate", "window": "5m" }, "agg": { "op": "sum", "by": ["route"] } } ],
  "top": { "n": 5, "rank": "per_step", "order": "desc" } }
```

PromQL: `topk(5, sum by (route) (rate(http_requests_total[5m])))`

`rank: "per_step"` is PromQL `topk`: the set can change from step to step. `rank: "max"`,
`"avg"` or `"last"` picks 5 series for the whole range instead, which usually reads better on a
chart. `order: "asc"` gives `bottomk`.

### 6. Show one metric for the top series of another

What is the p99 latency of the 10 busiest routes?

```json
{ "tqs": "3", "id": "latency-of-busiest", "kind": "timeseries", "time": { "from": "-1h", "step": "1m" },
  "queries": [
    { "name": "p99", "metric": "http_request_duration_seconds",
      "fn": { "op": "histogram_quantile", "q": 0.99, "window": "5m" }, "agg": { "op": "sum", "by": ["route"] } },
    { "name": "rps", "metric": "http_requests_total",
      "fn": { "op": "rate", "window": "5m" }, "agg": { "op": "sum", "by": ["route"] }, "hidden": true } ],
  "top": { "n": 10, "query": "rps" } }
```

PromQL: `histogram_quantile(0.99, sum by (route, le) (rate(http_request_duration_seconds_bucket[5m]))) and on(route) topk(10, sum by (route) (rate(http_requests_total[5m])))`

`top.query` ranks by another (usually hidden) query at each step and keeps only those keys in
every output.

### 7. Compare with yesterday (offset)

How does each job's traffic compare with the same time yesterday?

```json
{ "tqs": "3", "id": "vs-yesterday", "kind": "timeseries", "time": { "from": "-6h", "step": "5m" },
  "queries": [
    { "name": "now", "metric": "http_requests_total",
      "fn": { "op": "rate", "window": "5m" }, "agg": { "op": "sum", "by": ["job"] } },
    { "name": "before", "metric": "http_requests_total", "offset": "1d",
      "fn": { "op": "rate", "window": "5m" }, "agg": { "op": "sum", "by": ["job"] } } ],
  "formulas": [{ "name": "change_pct", "expr": "100 * (now - before) / before" }] }
```

PromQL: `sum by (job) (rate(http_requests_total[5m] offset 1d))` for `before`, and the ratio as
a formula over both.

`offset` shifts the evaluation into the past: the point at `t` reads `(t - 1d - 5m, t - 1d]`. It
takes a fixed positive duration; variables are not accepted there. This document returns three
outputs: `now`, `before` and `change_pct`.

### 8. Keep only values above a threshold (having)

Which routes serve more than 4 requests per second?

```json
{ "tqs": "3", "id": "busy-routes", "kind": "timeseries", "time": { "from": "-1h", "step": "1m" },
  "queries": [
    { "name": "rps", "metric": "http_requests_total",
      "fn": { "op": "rate", "window": "5m" }, "agg": { "op": "sum", "by": ["route"] },
      "having": { "op": ">", "value": 4 } } ] }
```

PromQL: `sum by (route) (rate(http_requests_total[5m])) > 4`

`having` drops points that fail the comparison (`=`, `!=`, `>`, `>=`, `<`, `<=`); it does not
turn them into 0 or 1.

### 9. Alert when a series is missing (absent)

Is the `api` job reporting at all?

```json
{ "tqs": "3", "id": "api-missing", "kind": "timeseries", "time": { "from": "-1h", "step": "1m" },
  "queries": [
    { "name": "missing", "metric": "up",
      "match": [{ "label": "job", "op": "=", "value": "api" }],
      "fn": { "op": "last" }, "agg": { "op": "absent" } } ] }
```

PromQL: `absent(up{job="api"})`

The result is `1` at every step where the selection has no series, labelled with the query's
`=` matchers (`job="api"`), and no point where it does. `fn: {"op": "last"}` reads the latest
sample within the 5-minute lookback, like a plain PromQL selector. `absent` takes no `by`,
`without` or `then`. For PromQL's `0 * absent(...)`, add a formula `missing * 0`.

### 10. Rewrite a label before aggregating (derive)

How much CPU do API pods use compared with worker pods, when the role is only in the pod name?

```json
{ "tqs": "3", "id": "cpu-by-role", "kind": "timeseries", "time": { "from": "-1h", "step": "1m" },
  "queries": [
    { "name": "cpu", "metric": "pod_cpu_seconds_total",
      "derive": [{ "label": "role", "from": "pod", "regex": "(api|worker)-.*", "replacement": "$1" }],
      "fn": { "op": "rate", "window": "5m" }, "agg": { "op": "sum", "by": ["role"] } } ] }
```

PromQL: `sum by (role) (label_replace(rate(pod_cpu_seconds_total[5m]), "role", "$1", "pod", "(api|worker)-.*"))`

`derive` rules run in order, before aggregation. When `regex` fully matches the value of `from`,
`label` is set to `replacement` with `$1` to `$9` expanded; otherwise the series keeps its
labels. Pods that match neither name end up in one series without a `role` label (key `""`).

### 11. Two-level aggregation (then)

What is the total queue depth per queue, counting each workspace's worker once?

```json
{ "tqs": "3", "id": "queue-depth", "kind": "timeseries", "time": { "from": "-6h", "step": "1m" },
  "queries": [
    { "name": "depth", "metric": "queue_depth",
      "fn": { "op": "last" },
      "agg": { "op": "max", "by": ["queue", "workspace"] },
      "then": { "op": "sum", "by": ["queue"] } } ] }
```

PromQL: `sum by (queue) (max by (queue, workspace) (queue_depth))`

`then` aggregates the groups produced by `agg` (after `having`). `then.by` must be a subset of
`agg.by`, because other labels no longer exist at that point.

### 12. Count groups that pass a filter

How many tenants sent at least one request today?

```json
{ "tqs": "3", "id": "active-tenants", "kind": "table", "time": { "from": "-24h" },
  "queries": [
    { "name": "tenants", "metric": "http_requests_total",
      "fn": { "op": "increase", "window": "$__range" },
      "agg": { "op": "sum", "by": ["tenant"] },
      "having": { "op": ">", "value": 0 },
      "then": { "op": "count" } } ] }
```

PromQL: `count(sum by (tenant) (increase(http_requests_total[$__range])) > 0)`

The order is `agg`, then `having`, then `then`. Without `by`, `then` collapses everything into
one series.

### 13. Keep every known key visible (outer join with coalesce)

Show the depth of every declared queue, with 0 for queues that have no depth samples.

```json
{ "tqs": "3", "id": "queues-at-zero", "kind": "timeseries", "time": { "from": "-6h", "step": "1m" },
  "queries": [
    { "name": "a", "metric": "queue_depth", "fn": { "op": "last" },
      "agg": { "op": "sum", "by": ["queue"] } },
    { "name": "b", "metric": "queue_declared_info", "fn": { "op": "last" },
      "agg": { "op": "sum", "by": ["queue"] } } ],
  "formulas": [{ "name": "depth", "expr": "coalesce(a, b * 0)", "join": "outer" }] }
```

PromQL: `sum by (queue) (queue_depth) or sum by (queue) (queue_declared_info * 0)`

With the default `join: "inner"`, a formula point exists only where every operand has one. With
`join: "outer"` it exists where any operand has one, and a missing operand is NULL, so
`coalesce` picks the first present value. This is how PromQL's `or` is written in TQS. Vector
operands of a formula must be grouped the same way; otherwise validation reports
`formula_join`.

## Spans

Spans queries filter span rows (`spans` conditions) and compute a `measure` per step. The point
at `t` covers spans whose start is in `(t - step, t]`. Fields: `service_name`, `span_name`,
`span_kind`, `status_code`, `duration_ms`, `region`, `trace_id`, `attr.<key>`,
`resource.<key>`. OTLP numbering applies: `span_kind` 2 is server, `status_code` 2 is error.

### 14. Span error rate by service

What percentage of each service's server spans fail?

```json
{ "tqs": "3", "id": "span-error-pct", "kind": "timeseries", "time": { "from": "-1h", "step": "1m" },
  "queries": [
    { "name": "e", "spans": [{ "field": "span_kind", "op": "=", "value": 2 }, { "field": "status_code", "op": "=", "value": 2 }],
      "measure": { "op": "rate" }, "agg": { "by": ["service_name"] }, "fill": "zero", "hidden": true },
    { "name": "a", "spans": [{ "field": "span_kind", "op": "=", "value": 2 }],
      "measure": { "op": "rate" }, "agg": { "by": ["service_name"] }, "hidden": true } ],
  "formulas": [{ "name": "pct", "expr": "100 * e / a" }] }
```

No PromQL equivalent. `measure.op: "rate"` is the span count divided by the step in seconds;
`count` gives the raw count. Spans queries group with `agg.by` only (no `agg.op`).

### 15. Span p99 latency

What is the exact p99 duration of server spans per service and route?

```json
{ "tqs": "3", "id": "span-p99", "kind": "timeseries", "time": { "from": "-1h", "step": "1m" },
  "queries": [
    { "name": "p99", "spans": [{ "field": "span_kind", "op": "=", "value": 2 }],
      "measure": { "op": "quantile", "field": "duration_ms", "q": 0.99 },
      "agg": { "by": ["service_name", "attr.http.route"] } } ] }
```

No PromQL equivalent. The quantile is computed exactly over the span durations in each step,
not estimated from buckets. `quantile`, `avg`, `min`, `max` and `sum` need
`"field": "duration_ms"`. You can group by `attr.<key>` and `resource.<key>`; spans without the
attribute fall into a group with an empty value.

## Logs

### 16. Log volume by severity

How many log lines per severity does the checkout service write?

```json
{ "tqs": "3", "id": "logs-by-severity", "kind": "timeseries", "time": { "from": "-6h", "step": "5m" },
  "queries": [
    { "name": "n", "logs": [{ "field": "service_name", "op": "=", "value": "checkout" }],
      "measure": { "op": "count" }, "agg": { "by": ["severity_text"] }, "fill": "zero" } ] }
```

No PromQL equivalent. Logs queries take `measure.op` `count` or `rate`. Fields:
`service_name`, `severity_number`, `severity_text`, `body`, `pattern_hash`, `trace_id`, `region`,
`attr.<key>`, `resource.<key>`.

### 17. Log search

Show recent error lines that mention a timeout in checkout or payments.

```json
{ "tqs": "3", "id": "timeout-errors", "kind": "log_search", "time": { "from": "-15m" },
  "search": {
    "logs": [
      { "field": "severity_number", "op": ">=", "value": 17 },
      { "field": "body", "op": "contains", "value": "timeout" },
      { "field": "service_name", "op": "in", "value": ["checkout", "payments"] } ],
    "order": "desc" },
  "limit": 100 }
```

All conditions must match. Severity numbers follow OTLP: 9 is INFO, 13 WARN, 17 ERROR, 21 FATAL.
`contains` is case-sensitive; use `=~` with a regex such as `(?i).*timeout.*` to ignore case.
Each row has `ts_ms`, `service_name`, `severity_text`, `body` and `trace_id`. `limit` is 1 to
5000 (default 100); `truncated: true` in the response means the limit was reached and more
rows may match.

### 18. Log patterns

What kinds of warnings and errors occur most often?

```json
{ "tqs": "3", "id": "error-patterns", "kind": "log_patterns", "time": { "from": "-1h" },
  "search": { "logs": [{ "field": "severity_number", "op": ">=", "value": 13 }] },
  "limit": 20 }
```

At ingest, each line's variable parts (numbers, UUIDs, IP addresses, long hex runs, quoted
strings) are masked and the result is hashed into `pattern_hash`. So `user 12 placed order 99`
and `user 7 placed order 123456` share a pattern. Each row has `pattern_hash`, `n` (line count),
`example` (one line) and `services`. Filter on `pattern_hash` in a `log_search` to see every line
of one pattern.

## Traces

### 19. Trace search

Find slow, failed traces that touched the cart routes of the checkout service.

```json
{ "tqs": "3", "id": "slow-failed-checkouts", "kind": "trace_search", "time": { "from": "-30m" },
  "search": {
    "spans": [
      { "field": "service_name", "op": "=", "value": "checkout" },
      { "field": "attr.http.route", "op": "=~", "value": "/cart.*" } ],
    "trace": [
      { "field": "duration_ms", "op": ">", "value": 500 },
      { "field": "has_error", "op": "=", "value": true } ],
    "order": "duration_desc" },
  "limit": 20 }
```

`search.spans`: at least one span of the trace matches all of them. `search.trace`: conditions
on the whole trace (`duration_ms`, `has_error`, `span_count`, `root.service_name`,
`root.span_name`, `service`). `order` is `duration_desc` or `start_desc`. Each row has
`trace_id`, `start_ms`, `duration_ms`, `span_count`, `has_error`, `root_service`, `root_span`
and `services`. Only spans that start inside the range are seen.

### 20. Trace waterfall

Show every span of one trace, indented by depth.

```json
{ "tqs": "3", "id": "waterfall", "kind": "trace", "trace_id": "5b8efff798038103d269b633813fc60c",
  "time": { "from": "-1h" } }
```

```bash
curl -s localhost:7070/api/v1/query -d @waterfall.json \
  | jq -r '.rows[] | "\("  " * (.depth|tonumber))\(.service_name) \(.span_name) \(.duration_ms)ms"'
```

`trace_id` is 32 lowercase hex characters. Each row has `depth`, `region`, `service_name`,
`span_name`, `offset_ms`, `duration_ms`, `self_ms` (duration minus children), `status_code`,
`is_root` and `orphan` (a span whose parent is missing, drawn at depth 0). `time` is optional for
this kind, but pass it: without it, the lookup probes every span file.

## PromQL to TQS translation

| PromQL | TQS 3 |
| --- | --- |
| `sum by (a) (rate(m{l="v"}[5m]))` | `metric: m, match: [{l = v}], fn: {rate, 5m}, agg: {sum, by: [a]}` |
| `histogram_quantile(0.95, sum by (a, le) (rate(m_bucket[5m])))` | `metric: m, fn: {histogram_quantile, q: 0.95, window: 5m}, agg: {sum, by: [a]}` |
| `x / y` (same grouping) | two queries + formula `x / y` |
| `x * 3600`, `clamp_max(x, 1)`, `clamp_min(x, 1)` | formula `x * 3600`, `least(x, 1)`, `greatest(x, 1)` |
| `x or vector(0)` | `fill: "zero"` on a single-series query |
| `a or b * 0 or c * 0` | formula `coalesce(a, b * 0, c * 0)`, `join: "outer"` |
| `0 * absent(m{l="v"})` | query `agg: {op: absent}` + formula `z * 0` |
| `sum by (a) (max by (a, b) (m))` | `agg: {max, by: [a, b]}, then: {sum, by: [a]}` |
| `count(count by (a) (increase(m[$__range]) > 0))` | `agg: {sum, by: [a]}, having: {> 0}, then: {count}` |
| `label_replace(…, "dst", "$1", "src", "re")` | `derive: [{label: dst, from: src, regex: re, replacement: "$1"}]` |
| `x and on(k) topk(10, y)` | `y` as hidden query; `top: {n: 10, query: y}` |
| `topk(n, x)`, `bottomk(n, x)` | `top: {n}`, `top: {n, order: asc}` |
| `time() - m` | formula `time - m` |
| `m offset 30m` | `offset: "30m"` |
| `x > 1` | `having: {op: ">", value: 1}` |

The shorthand in the right column abbreviates the JSON; for example `fn: {rate, 5m}` means
`"fn": {"op": "rate", "window": "5m"}`.

Not supported: subqueries (`[range:step]`), the `@` modifier, `unless`, many-to-one matching
(`group_left`, `group_right`), `on` or `ignoring` outside the `and on(...) topk` pattern,
`label_join`, `predict_linear`, `deriv`, `holt_winters`, `quantile_over_time`, `stddev`, `stdvar`,
`count_values`, `quantile` across series, `sort`, `timestamp`, math functions beyond the formula
set, `bool` comparisons, native histograms, and an aggregation applied after a formula (rewrite
it before the formula, for example `max by (app) (time() - x)` as `time - min by (app) (x)`).
