// OpenTelemetry setup, loaded before the app with `node --import ./telemetry.js`.
// Everything goes to tiny-eye's OTLP/HTTP endpoint (protobuf): /v1/traces, /v1/metrics, /v1/logs.
// Configure with the standard env vars: OTEL_EXPORTER_OTLP_ENDPOINT (default http://127.0.0.1:7070),
// OTEL_SERVICE_NAME (set per process by launch.js).
import { NodeSDK } from "@opentelemetry/sdk-node";
import { getNodeAutoInstrumentations } from "@opentelemetry/auto-instrumentations-node";
import { OTLPTraceExporter } from "@opentelemetry/exporter-trace-otlp-proto";
import { OTLPMetricExporter } from "@opentelemetry/exporter-metrics-otlp-proto";
import { OTLPLogExporter } from "@opentelemetry/exporter-logs-otlp-proto";
import { PeriodicExportingMetricReader } from "@opentelemetry/sdk-metrics";
import { BatchLogRecordProcessor } from "@opentelemetry/sdk-logs";

process.env.OTEL_EXPORTER_OTLP_ENDPOINT ||= "http://127.0.0.1:7070";
process.env.OTEL_METRIC_EXPORT_INTERVAL ||= "10000"; // the SDK default is 60 s

const sdk = new NodeSDK({
  traceExporter: new OTLPTraceExporter(),
  // Cumulative temporality (the SDK default) is what tiny-eye stores; export every 10 s.
  metricReader: new PeriodicExportingMetricReader({ exporter: new OTLPMetricExporter(), exportIntervalMillis: 10_000 }),
  logRecordProcessors: [new BatchLogRecordProcessor({ exporter: new OTLPLogExporter() })],
  instrumentations: [getNodeAutoInstrumentations({ "@opentelemetry/instrumentation-fs": { enabled: false } })],
});
sdk.start();

for (const sig of ["SIGINT", "SIGTERM"]) {
  process.on(sig, () => sdk.shutdown().finally(() => process.exit(0)));
}
