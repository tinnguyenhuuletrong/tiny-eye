// OpenTelemetry setup, imported first by server.ts. Everything goes to tiny-eye's OTLP/HTTP
// endpoint (protobuf): /v1/traces, /v1/metrics, /v1/logs.
// Configure with the standard env vars: OTEL_EXPORTER_OTLP_ENDPOINT (default http://127.0.0.1:7070),
// OTEL_SERVICE_NAME (set per process by launch.ts).
// No auto-instrumentation: Bun.serve does not go through node:http, so server.ts emits its own
// server/client spans and the http.server.request.duration metric.

import { OTLPLogExporter } from "@opentelemetry/exporter-logs-otlp-proto";
import { OTLPMetricExporter } from "@opentelemetry/exporter-metrics-otlp-proto";
import { OTLPTraceExporter } from "@opentelemetry/exporter-trace-otlp-proto";
import { BatchLogRecordProcessor } from "@opentelemetry/sdk-logs";
import { PeriodicExportingMetricReader } from "@opentelemetry/sdk-metrics";
import { NodeSDK } from "@opentelemetry/sdk-node";

process.env.OTEL_EXPORTER_OTLP_ENDPOINT ||= "http://127.0.0.1:7070";

const sdk = new NodeSDK({
  traceExporter: new OTLPTraceExporter(),
  // Cumulative temporality (the SDK default) is what tiny-eye stores; export every 10 s (default is 60 s).
  metricReaders: [new PeriodicExportingMetricReader({ exporter: new OTLPMetricExporter(), exportIntervalMillis: 10_000 })],
  logRecordProcessors: [new BatchLogRecordProcessor({ exporter: new OTLPLogExporter() })],
});
sdk.start();

for (const sig of ["SIGINT", "SIGTERM"] as const) {
  process.on(sig, () => sdk.shutdown().finally(() => process.exit(0)));
}
