---
title: Sending data
description: Point OpenTelemetry SDKs, the OTel Collector, Prometheus, Grafana Alloy and Loki clients at tiny-eye, and learn how incoming data is mapped.
sidebar_position: 3
---

tiny-eye accepts three protocols on its HTTP port (7070 by default). This page shows how to
point common senders at it, how incoming data is translated, and which rows are refused.

| Protocol | Endpoint | Body | Success |
| --- | --- | --- | --- |
| OTLP/HTTP metrics | `POST /v1/metrics` | protobuf or JSON, optional gzip | `200` |
| OTLP/HTTP traces | `POST /v1/traces` | protobuf or JSON, optional gzip | `200` |
| OTLP/HTTP logs | `POST /v1/logs` | protobuf or JSON, optional gzip | `200` |
| Prometheus remote-write 1.0 | `POST /api/v1/write` | snappy-compressed protobuf `WriteRequest` | `204` |
| Loki push | `POST /loki/api/v1/push` | JSON, optional gzip | `204` |

A success status means the rows are in the write-ahead log and queryable. `400` means the body
could not be decoded (the response text says why). `429` means the ingest arena is full
(`ingest.arena_limit`); retry later. With auth enabled, every ingest endpoint needs a bearer
token with the `ingest` scope (see [Security](../deep-dive/security.md)).

:::warning What is tested
The [demo shop](demo-shop.md) exercises OTLP/HTTP protobuf from the OpenTelemetry Node.js SDK.
`make e2e` exercises Prometheus remote-write. OTLP JSON and Loki JSON are covered by unit tests
of the decoders. The OTel Collector, Prometheus and Alloy configurations below are standard
configurations for those tools, but no tiny-eye test runs them.
:::

## OpenTelemetry SDKs

Every OpenTelemetry SDK reads the standard environment variables. Point the OTLP exporter at
tiny-eye and choose an HTTP protocol:

```bash
export OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:7070
export OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf   # or http/json
export OTEL_SERVICE_NAME=checkout
```

The SDK appends `/v1/traces`, `/v1/metrics` and `/v1/logs` to the endpoint.

- **gRPC is not supported.** Do not use `OTEL_EXPORTER_OTLP_PROTOCOL=grpc` or port 4317.
- **Keep cumulative temporality** for metrics (the default in most SDKs). Delta points are
  skipped.
- **Set histogram buckets in the unit you record.** Some SDKs default to millisecond-sized
  buckets (0, 5, 10, 25, …); for a histogram recorded in seconds, set explicit bucket boundaries
  in seconds, or every quantile lands in the first bucket.

With auth enabled, add the token as a header:

```bash
export OTEL_EXPORTER_OTLP_HEADERS="Authorization=Bearer ${TINY_EYE_INGEST_TOKEN}"
```

## OpenTelemetry Collector

Use the `otlphttp` exporter. It appends the `/v1/...` paths itself.

```yaml
receivers:
  otlp:
    protocols:
      grpc: {}
      http: {}

exporters:
  otlphttp/tiny-eye:
    endpoint: http://tiny-eye.internal:7070
    encoding: proto            # or json
    # headers:
    #   Authorization: "Bearer ${env:TINY_EYE_INGEST_TOKEN}"

service:
  pipelines:
    traces:  { receivers: [otlp], exporters: [otlphttp/tiny-eye] }
    metrics: { receivers: [otlp], exporters: [otlphttp/tiny-eye] }
    logs:    { receivers: [otlp], exporters: [otlphttp/tiny-eye] }
```

The Collector can receive gRPC from your applications and forward over HTTP, which is a way to
serve gRPC-only senders today. If your pipeline produces delta metrics, add the
`deltatocumulative` processor before the exporter.

## Prometheus and Grafana Alloy (remote-write)

tiny-eye speaks remote-write 1.0, the default message format of Prometheus and Alloy.

Prometheus (`prometheus.yml`):

```yaml
remote_write:
  - url: http://tiny-eye.internal:7070/api/v1/write
    # authorization:
    #   credentials_file: /etc/prometheus/tiny-eye-token
```

Grafana Alloy:

```text
prometheus.remote_write "tiny_eye" {
  endpoint {
    url = "http://tiny-eye.internal:7070/api/v1/write"
  }
}
```

Do not switch to remote-write 2.0 (`protobuf_message: io.prometheus.write.v2.Request` in
Prometheus); tiny-eye decodes 1.0 only. Remote-written metrics keep their names and labels as
sent. Their type shows as `unknown` in the catalog, because remote-write 1.0 does not carry it.

To monitor tiny-eye itself, scrape its `/metrics` endpoint with any of these agents and
remote-write the result back. `dashboards/self.yaml` expects the scrape job to be named
`tiny-eye`.

## Loki push (JSON only)

`POST /loki/api/v1/push` accepts the Loki JSON body. Each value is
`["<unix ns as string>", "<line>"]`, optionally with a third element of structured metadata:

```bash
NOW=$(date +%s)000000000
curl -s -X POST http://127.0.0.1:7070/loki/api/v1/push \
  -H 'Content-Type: application/json' \
  -d '{"streams":[{"stream":{"service_name":"web","level":"warn"},
       "values":[["'"$NOW"'","slow request 120ms",{"trace_id":"5b8efff798038103d269b633813fc60c"}]]}]}'
```

How a stream maps to a log row:

- `service_name` comes from the first stream label present among `service_name`, `service`,
  `app`, `job`; otherwise `unknown_service`.
- The severity comes from `level`, `detected_level` or `severity` (`warn` becomes
  `severity_number` 13 and `severity_text` `WARN`).
- Stream labels become resource attributes; structured metadata becomes log attributes. A
  32-hex `trace_id` in the metadata links the line to its trace.

:::warning
Protobuf push is not supported. Promtail and Alloy's `loki.write` send snappy-compressed
protobuf, so they cannot push to tiny-eye today. Ship logs from those agents over OTLP instead
(for example, Alloy's `otelcol.exporter.otlphttp`).
:::

## How OTLP metrics become Prometheus series

TQS reproduces PromQL on Prometheus-shaped data, so the OTLP receiver translates metrics into
that shape:

| OTLP | Stored as |
| --- | --- |
| Metric name `http.server.request.duration` | `http_server_request_duration`: every character outside `[a-zA-Z0-9_:]` becomes `_`. No unit suffix is added. |
| Attribute key `http.route` | Label `http_route`: every character outside `[a-zA-Z0-9_]` becomes `_`. Empty values are dropped. |
| Resource `service.name` (and `service.namespace`) | Label `job`: `<namespace>/<name>`, or `<name>` without a namespace. |
| Resource `service.instance.id` | Label `instance`. |
| Other resource attributes | Not added as labels. |
| Gauge | Same name, type gauge. |
| Monotonic cumulative sum | `<name>_total` (unless the name already ends in `_total`), type counter. |
| Non-monotonic sum | Same name, type gauge. |
| Explicit-bucket histogram | `<name>_bucket{le="…"}` with cumulative counts (last bucket `le="+Inf"`), `<name>_count`, `<name>_sum`. |
| Summary | `<name>{quantile="…"}`, `<name>_count`, `<name>_sum`. |
| Delta temporality (sums, histograms) | Skipped. |
| Exponential histogram | Skipped. |

For a histogram, TQS reads `<name>_bucket` when you write `"metric": "<name>"` with
`histogram_quantile`, so you name the base metric, not the `_bucket` series.

Skipped points are counted and logged at debug level; the request still succeeds. The
`/api/v1/catalog` endpoint lists the resulting metric names and label keys, which is the
quickest way to see what your SDK produced.

For spans and logs, `service_name` comes from the resource's `service.name` (or
`unknown_service`). Spans with an all-zero trace id are dropped. Span events and links are not
stored yet. A log record without `timeUnixNano` uses `observedTimeUnixNano`; a missing severity
text is derived from the severity number.

## Timestamps outside the accepted window

Each row's timestamp must fall within `[now - ingest.max_sample_age, now + ingest.max_sample_future]`
(1 hour back, 10 minutes ahead by default). Rows outside it are dropped at ingest; the rest of
the request is accepted and the response is still a success. Dropped rows are counted:

```bash
curl -s http://127.0.0.1:7070/metrics | grep rejected_rows_total
# tiny_eye_rejected_rows_total{reason="out_of_bounds"} 0
```

To backfill older data, raise `ingest.max_sample_age` (for example
`TINY_EYE__INGEST__MAX_SAMPLE_AGE=7d`) for the duration of the import.

## Redaction of secret attributes

At ingest, tiny-eye replaces the value of secret-looking attributes with `<redacted>`. A key
matches when, in lower case, it equals one of `authorization`, `cookie`, `set-cookie`,
`password`, `passwd`, `token`, `api_key`, `apikey`, `secret`, or ends with `.` or `_` followed by
one of them (for example `http.request.header.authorization` or `db_password`).

Redaction applies to OTLP resource, span, log and metric point attributes, and to Loki stream
labels and structured metadata. It does not inspect log bodies or remote-write labels: keep
secrets out of log lines at the source.
