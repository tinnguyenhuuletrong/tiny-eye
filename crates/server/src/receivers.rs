//! Ingest protocols → engine rows (SPEC §5.1 step 1): Prometheus remote-write 1.0,
//! OTLP/HTTP (protobuf or JSON) for metrics, traces and logs, and Loki push (JSON).

use anyhow::{Context, Result, bail};
use engine::Rows;
use engine::ids::{PATTERN_VERSION, pattern_hash, series_id};
use engine::tables::*;
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue, any_value};
use opentelemetry_proto::tonic::metrics::v1::{AggregationTemporality, metric::Data, number_data_point};
use prost::Message;

/// Metric types stored in `metrics_series.metric_type`.
pub const TYPE_UNKNOWN: u8 = 0;
pub const TYPE_COUNTER: u8 = 1;
pub const TYPE_GAUGE: u8 = 2;
pub const TYPE_HISTOGRAM: u8 = 3;
pub const TYPE_SUMMARY: u8 = 4;

/// Prometheus' staleness marker (a specific NaN payload).
const STALE_NAN: u64 = 0x7ff0_0000_0000_0002;
/// OTLP `DataPointFlags.FLAG_NO_RECORDED_VALUE`.
const NO_RECORDED_VALUE: u32 = 1;

/// Attribute keys whose values are replaced at ingest (SPEC §11).
const SECRET_KEYS: &[&str] = &["authorization", "cookie", "set-cookie", "password", "passwd", "token", "api_key", "apikey", "secret"];

fn redact(key: &str, value: String) -> String {
    let k = key.to_ascii_lowercase();
    if SECRET_KEYS.iter().any(|s| k == *s || k.ends_with(&format!(".{s}")) || k.ends_with(&format!("_{s}"))) {
        "<redacted>".into()
    } else {
        value
    }
}

/// Largest decompressed request body; a small compressed body cannot expand past this.
pub const MAX_DECOMPRESSED: usize = 256 << 20;

/// Snappy block decode with the declared length checked before allocating.
fn snappy(body: &[u8]) -> Result<Vec<u8>> {
    let n = snap::raw::decompress_len(body).context("snappy header")?;
    if n > MAX_DECOMPRESSED {
        bail!("snappy body expands to {n} bytes; the limit is {MAX_DECOMPRESSED}");
    }
    snap::raw::Decoder::new().decompress_vec(body).context("snappy body")
}

/// Request bodies may be gzip-compressed (OTLP/HTTP `Content-Encoding: gzip`).
pub fn decompress(encoding: Option<&str>, body: &[u8]) -> Result<Vec<u8>> {
    match encoding {
        Some("gzip") => {
            let mut out = Vec::new();
            let mut limited = std::io::Read::take(flate2::read::GzDecoder::new(body), MAX_DECOMPRESSED as u64 + 1);
            std::io::Read::read_to_end(&mut limited, &mut out).context("gzip body")?;
            if out.len() > MAX_DECOMPRESSED {
                bail!("gzip body expands past {MAX_DECOMPRESSED} bytes");
            }
            Ok(out)
        }
        Some("snappy") => snappy(body),
        None | Some("identity") => Ok(body.to_vec()),
        Some(e) => bail!("unsupported Content-Encoding {e:?}"),
    }
}

// ---------------------------------------------------------------- shared metric helpers

struct MetricSink<'a> {
    rows: &'a mut Rows,
    tenant: &'a str,
    region: &'a str,
}

impl MetricSink<'_> {
    fn push(&mut self, name: &str, metric_type: u8, unit: Option<&str>, labels: Labels, ts_ms: i64, value: f64) {
        let id = series_id(self.tenant, self.region, name, &labels);
        self.rows.series.push(MetricsSeriesRow {
            day_ms: ts_ms.div_euclid(86_400_000) * 86_400_000,
            series_id: id,
            metric_name: name.to_string(),
            metric_type,
            unit: unit.filter(|u| !u.is_empty()).map(str::to_string),
            labels,
            ..Default::default()
        });
        let is_stale = value.to_bits() == STALE_NAN;
        self.rows.points.push(MetricsPointsRow {
            series_id: id,
            timestamp_ms: ts_ms,
            value: if is_stale { f64::NAN } else { value },
            is_stale,
            ..Default::default()
        });
    }
}

/// Prometheus label-name rules: `[a-zA-Z_][a-zA-Z0-9_]*`.
pub fn sanitize_label(k: &str) -> String {
    let mut s: String = k.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' }).collect();
    if s.chars().next().is_none_or(|c| c.is_ascii_digit()) {
        s.insert(0, '_');
    }
    s
}

/// Metric-name rules: `[a-zA-Z_:][a-zA-Z0-9_:]*`.
pub fn sanitize_metric(k: &str) -> String {
    let mut s: String = k.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == ':' { c } else { '_' }).collect();
    if s.chars().next().is_none_or(|c| c.is_ascii_digit()) {
        s.insert(0, '_');
    }
    s
}

// ---------------------------------------------------------------- Prometheus remote-write 1.0

#[derive(Clone, PartialEq, Message)]
pub struct WriteRequest {
    #[prost(message, repeated, tag = "1")]
    pub timeseries: Vec<TimeSeries>,
}

#[derive(Clone, PartialEq, Message)]
pub struct TimeSeries {
    #[prost(message, repeated, tag = "1")]
    pub labels: Vec<Label>,
    #[prost(message, repeated, tag = "2")]
    pub samples: Vec<Sample>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Label {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub value: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct Sample {
    #[prost(double, tag = "1")]
    pub value: f64,
    #[prost(int64, tag = "2")]
    pub timestamp: i64,
}

/// Snappy-compressed `WriteRequest` → rows. `__name__` is the metric; empty labels are dropped
/// (an empty label value is the same as an absent label, SPEC §4.3).
pub fn prom_remote_write(body: &[u8], tenant: &str, region: &str) -> Result<Rows> {
    let raw = snappy(body).context("remote-write body is not snappy block format")?;
    let req = WriteRequest::decode(raw.as_slice()).context("remote-write body is not a WriteRequest")?;
    let mut rows = Rows::default();
    let mut sink = MetricSink { rows: &mut rows, tenant, region };
    for ts in req.timeseries {
        let mut name = String::new();
        let mut labels = Labels::new();
        for l in ts.labels {
            if l.name == "__name__" {
                name = l.value;
            } else if !l.value.is_empty() {
                labels.insert(l.name, l.value);
            }
        }
        if name.is_empty() {
            continue;
        }
        for s in ts.samples {
            sink.push(&name, TYPE_UNKNOWN, None, labels.clone(), s.timestamp, s.value);
        }
    }
    Ok(rows)
}

/// Encode a `WriteRequest` the way Prometheus sends it (tests, benchmarks).
pub fn encode_remote_write(req: &WriteRequest) -> Vec<u8> {
    snap::raw::Encoder::new().compress_vec(&req.encode_to_vec()).expect("snappy encode")
}

// ---------------------------------------------------------------- OTLP

fn any_to_string(v: &Option<AnyValue>) -> String {
    match v.as_ref().and_then(|v| v.value.as_ref()) {
        None => String::new(),
        Some(any_value::Value::StringValue(s)) => s.clone(),
        Some(any_value::Value::BoolValue(b)) => b.to_string(),
        Some(any_value::Value::IntValue(i)) => i.to_string(),
        Some(any_value::Value::DoubleValue(d)) => d.to_string(),
        Some(any_value::Value::BytesValue(b)) => b.iter().map(|x| format!("{x:02x}")).collect(),
        Some(other) => serde_json::to_string(&format!("{other:?}")).unwrap_or_default(),
    }
}

fn attrs(kvs: &[KeyValue]) -> Labels {
    kvs.iter().map(|kv| (kv.key.clone(), redact(&kv.key, any_to_string(&kv.value)))).collect()
}

fn service_name(resource: &Labels) -> String {
    resource.get("service.name").cloned().unwrap_or_else(|| "unknown_service".into())
}

fn trace_id(b: &[u8]) -> Option<[u8; 16]> {
    (b.len() == 16 && b.iter().any(|x| *x != 0)).then(|| b.try_into().expect("16 bytes"))
}

fn span_id(b: &[u8]) -> Option<u64> {
    (b.len() == 8 && b.iter().any(|x| *x != 0)).then(|| u64::from_be_bytes(b.try_into().expect("8 bytes")))
}

fn decode<T: Message + Default + serde::de::DeserializeOwned>(content_type: Option<&str>, body: &[u8]) -> Result<T> {
    if content_type.is_some_and(|c| c.starts_with("application/json")) {
        Ok(serde_json::from_slice(body).context("OTLP JSON body")?)
    } else {
        Ok(T::decode(body).context("OTLP protobuf body")?)
    }
}

/// OTLP metrics → Prometheus-shaped series (the shape TQS reproduces PromQL on):
/// `job`/`instance` from `service.name`/`service.instance.id`; monotonic cumulative sums get
/// `_total`; histograms become `_bucket{le}` (cumulative) + `_sum` + `_count`; summaries
/// `{quantile}` + `_sum` + `_count`. Delta temporality and exponential histograms are counted
/// in the returned `skipped`.
pub fn otlp_metrics(content_type: Option<&str>, body: &[u8], tenant: &str, region: &str) -> Result<(Rows, usize)> {
    let req: ExportMetricsServiceRequest = decode(content_type, body)?;
    let mut rows = Rows::default();
    let mut skipped = 0;
    let mut sink = MetricSink { rows: &mut rows, tenant, region };
    for rm in &req.resource_metrics {
        let res = rm.resource.as_ref().map(|r| attrs(&r.attributes)).unwrap_or_default();
        let mut base = Labels::new();
        if let Some(s) = res.get("service.name") {
            let job = match res.get("service.namespace") {
                Some(ns) => format!("{ns}/{s}"),
                None => s.clone(),
            };
            base.insert("job".into(), job);
        }
        if let Some(i) = res.get("service.instance.id") {
            base.insert("instance".into(), i.clone());
        }
        for sm in &rm.scope_metrics {
            for m in &sm.metrics {
                let name = sanitize_metric(&m.name);
                let unit = Some(m.unit.as_str());
                let labels_of = |kvs: &[KeyValue]| {
                    let mut l = base.clone();
                    for (k, v) in attrs(kvs) {
                        if !v.is_empty() {
                            l.insert(sanitize_label(&k), v);
                        }
                    }
                    l
                };
                // OTLP's "no recorded value" flag is the push-side staleness marker: the series ended
                // (a process exited, a pod went away). It becomes the same marker remote-write uses,
                // so queries stop carrying the last value forward.
                let stale = |flags: u32| flags & NO_RECORDED_VALUE != 0;
                let num = |flags: u32, v: &Option<number_data_point::Value>| match v {
                    _ if stale(flags) => f64::from_bits(STALE_NAN),
                    Some(number_data_point::Value::AsDouble(d)) => *d,
                    Some(number_data_point::Value::AsInt(i)) => *i as f64,
                    None => f64::NAN,
                };
                let ms = |ns: u64| (ns / 1_000_000) as i64;
                match &m.data {
                    Some(Data::Gauge(g)) => {
                        for p in &g.data_points {
                            sink.push(&name, TYPE_GAUGE, unit, labels_of(&p.attributes), ms(p.time_unix_nano), num(p.flags, &p.value));
                        }
                    }
                    Some(Data::Sum(s)) => {
                        if s.aggregation_temporality == AggregationTemporality::Delta as i32 {
                            skipped += s.data_points.len();
                            continue;
                        }
                        let (n, ty) = if s.is_monotonic {
                            (if name.ends_with("_total") { name.clone() } else { format!("{name}_total") }, TYPE_COUNTER)
                        } else {
                            (name.clone(), TYPE_GAUGE)
                        };
                        for p in &s.data_points {
                            sink.push(&n, ty, unit, labels_of(&p.attributes), ms(p.time_unix_nano), num(p.flags, &p.value));
                        }
                    }
                    Some(Data::Histogram(h)) => {
                        if h.aggregation_temporality == AggregationTemporality::Delta as i32 {
                            skipped += h.data_points.len();
                            continue;
                        }
                        for p in &h.data_points {
                            let l = labels_of(&p.attributes);
                            let t = ms(p.time_unix_nano);
                            if stale(p.flags) {
                                let marker = f64::from_bits(STALE_NAN);
                                for le in p.explicit_bounds.iter().map(|b| fmt_le(*b)).chain(["+Inf".to_string()]) {
                                    let mut lb = l.clone();
                                    lb.insert("le".into(), le);
                                    sink.push(&format!("{name}_bucket"), TYPE_HISTOGRAM, unit, lb, t, marker);
                                }
                                sink.push(&format!("{name}_count"), TYPE_HISTOGRAM, unit, l.clone(), t, marker);
                                sink.push(&format!("{name}_sum"), TYPE_HISTOGRAM, unit, l, t, marker);
                                continue;
                            }
                            let mut cum = 0u64;
                            for (i, c) in p.bucket_counts.iter().enumerate() {
                                cum += c;
                                let mut lb = l.clone();
                                lb.insert("le".into(), p.explicit_bounds.get(i).map(|b| fmt_le(*b)).unwrap_or_else(|| "+Inf".into()));
                                sink.push(&format!("{name}_bucket"), TYPE_HISTOGRAM, unit, lb, t, cum as f64);
                            }
                            sink.push(&format!("{name}_count"), TYPE_HISTOGRAM, unit, l.clone(), t, p.count as f64);
                            if let Some(sum) = p.sum {
                                sink.push(&format!("{name}_sum"), TYPE_HISTOGRAM, unit, l, t, sum);
                            }
                        }
                    }
                    Some(Data::Summary(s)) => {
                        for p in &s.data_points {
                            let l = labels_of(&p.attributes);
                            let t = ms(p.time_unix_nano);
                            for q in &p.quantile_values {
                                let mut lq = l.clone();
                                lq.insert("quantile".into(), fmt_le(q.quantile));
                                sink.push(&name, TYPE_SUMMARY, unit, lq, t, q.value);
                            }
                            sink.push(&format!("{name}_count"), TYPE_SUMMARY, unit, l.clone(), t, p.count as f64);
                            sink.push(&format!("{name}_sum"), TYPE_SUMMARY, unit, l, t, p.sum);
                        }
                    }
                    Some(Data::ExponentialHistogram(e)) => skipped += e.data_points.len(),
                    None => {}
                }
            }
        }
    }
    Ok((rows, skipped))
}

/// Prometheus formats `le`/`quantile` like Go's `strconv.FormatFloat(f, 'g', -1, 64)` for these values.
fn fmt_le(v: f64) -> String {
    if v.is_infinite() { "+Inf".into() } else { format!("{v}") }
}

pub fn otlp_traces(content_type: Option<&str>, body: &[u8]) -> Result<Rows> {
    let req: ExportTraceServiceRequest = decode(content_type, body)?;
    let mut rows = Rows::default();
    for rs in &req.resource_spans {
        let res = rs.resource.as_ref().map(|r| attrs(&r.attributes)).unwrap_or_default();
        let svc = service_name(&res);
        for ss in &rs.scope_spans {
            for s in &ss.spans {
                let Some(tid) = trace_id(&s.trace_id) else { continue };
                let status = s.status.as_ref();
                rows.spans.push(SpansRow {
                    trace_id: tid,
                    span_id: span_id(&s.span_id).unwrap_or(0),
                    parent_span_id: span_id(&s.parent_span_id),
                    start_time_ns: s.start_time_unix_nano as i64,
                    duration_ns: s.end_time_unix_nano.saturating_sub(s.start_time_unix_nano),
                    service_name: svc.clone(),
                    span_name: s.name.clone(),
                    span_kind: s.kind.clamp(0, 255) as u8,
                    status_code: status.map(|st| st.code.clamp(0, 255) as u8).unwrap_or(0),
                    status_message: status.map(|st| st.message.clone()).filter(|m| !m.is_empty()),
                    resource_attrs: res.clone(),
                    span_attrs: attrs(&s.attributes),
                    ..Default::default()
                });
            }
        }
    }
    Ok(rows)
}

fn severity_text_of(n: u8) -> &'static str {
    match n {
        21.. => "FATAL",
        17.. => "ERROR",
        13.. => "WARN",
        9.. => "INFO",
        5.. => "DEBUG",
        1.. => "TRACE",
        0 => "UNSPECIFIED",
    }
}

/// Fill the derived columns of a log row: severity text (if absent) and the pattern hash.
fn finish_log(mut row: LogsRow) -> LogsRow {
    if row.severity_text.is_empty() {
        row.severity_text = severity_text_of(row.severity_number).into();
    }
    row.pattern_hash = pattern_hash(&row.body);
    row.pattern_version = PATTERN_VERSION;
    row
}

pub fn otlp_logs(content_type: Option<&str>, body: &[u8]) -> Result<Rows> {
    let req: ExportLogsServiceRequest = decode(content_type, body)?;
    let mut rows = Rows::default();
    for rl in &req.resource_logs {
        let res = rl.resource.as_ref().map(|r| attrs(&r.attributes)).unwrap_or_default();
        let svc = service_name(&res);
        for sl in &rl.scope_logs {
            for l in &sl.log_records {
                let ts = if l.time_unix_nano > 0 { l.time_unix_nano } else { l.observed_time_unix_nano };
                rows.logs.push(finish_log(LogsRow {
                    timestamp_ns: ts as i64,
                    service_name: svc.clone(),
                    severity_number: l.severity_number.clamp(0, 255) as u8,
                    severity_text: l.severity_text.clone(),
                    body: any_to_string(&l.body),
                    trace_id: trace_id(&l.trace_id),
                    span_id: span_id(&l.span_id),
                    resource_attrs: res.clone(),
                    log_attrs: attrs(&l.attributes),
                    ..Default::default()
                }));
            }
        }
    }
    Ok(rows)
}

// ---------------------------------------------------------------- Loki push (JSON)

#[derive(serde::Deserialize)]
struct LokiPush {
    streams: Vec<LokiStream>,
}

#[derive(serde::Deserialize)]
struct LokiStream {
    stream: Labels,
    values: Vec<Vec<serde_json::Value>>,
}

fn level_number(level: &str) -> u8 {
    match level.to_ascii_lowercase().as_str() {
        "trace" => 1,
        "debug" | "dbug" => 5,
        "info" | "information" => 9,
        "warn" | "warning" => 13,
        "error" | "err" | "eror" => 17,
        "fatal" | "critical" | "crit" | "panic" => 21,
        _ => 0,
    }
}

pub fn loki_push(body: &[u8]) -> Result<Rows> {
    let req: LokiPush = serde_json::from_slice(body).context("Loki push body (JSON)")?;
    let mut rows = Rows::default();
    for s in req.streams {
        let res: Labels = s.stream.iter().map(|(k, v)| (k.clone(), redact(k, v.clone()))).collect();
        let svc =
            ["service_name", "service", "app", "job"].iter().find_map(|k| res.get(*k)).cloned().unwrap_or_else(|| "unknown_service".into());
        let level = ["level", "detected_level", "severity"].iter().find_map(|k| res.get(*k)).cloned().unwrap_or_default();
        for v in s.values {
            let ts: i64 =
                v.first().and_then(|x| x.as_str()).and_then(|x| x.parse().ok()).context("Loki value needs [\"<unix ns>\", \"<line>\"]")?;
            let line = v.get(1).and_then(|x| x.as_str()).unwrap_or_default().to_string();
            let meta: Labels = v
                .get(2)
                .and_then(|m| m.as_object())
                .map(|m| m.iter().map(|(k, x)| (k.clone(), redact(k, x.as_str().unwrap_or_default().to_string()))).collect())
                .unwrap_or_default();
            let tid = meta.get("trace_id").and_then(|h| hex16(h));
            rows.logs.push(finish_log(LogsRow {
                timestamp_ns: ts,
                service_name: svc.clone(),
                severity_number: level_number(&level),
                severity_text: level.to_uppercase(),
                body: line,
                trace_id: tid,
                resource_attrs: res.clone(),
                log_attrs: meta,
                ..Default::default()
            }));
        }
    }
    Ok(rows)
}

fn hex16(h: &str) -> Option<[u8; 16]> {
    if h.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_write_round_trip() {
        let req = WriteRequest {
            timeseries: vec![TimeSeries {
                labels: vec![
                    Label { name: "__name__".into(), value: "up".into() },
                    Label { name: "job".into(), value: "api".into() },
                    Label { name: "empty".into(), value: "".into() },
                ],
                samples: vec![Sample { value: 1.0, timestamp: 1000 }, Sample { value: f64::from_bits(STALE_NAN), timestamp: 2000 }],
            }],
        };
        let rows = prom_remote_write(&encode_remote_write(&req), "t", "r").expect("decode");
        assert_eq!(rows.points.len(), 2);
        assert!(rows.points[1].is_stale && !rows.points[0].is_stale);
        assert_eq!(rows.series[0].labels.len(), 1, "empty label dropped");
        assert_eq!(rows.series[0].metric_name, "up");
    }

    #[test]
    fn otlp_no_recorded_value_is_a_staleness_marker() {
        // A pusher marks a series as ended (the process exited) with FLAG_NO_RECORDED_VALUE.
        let body = serde_json::json!({"resourceMetrics": [{"resource": {"attributes": [{"key": "service.name", "value": {"stringValue": "machine"}}]},
            "scopeMetrics": [{"metrics": [
              {"name": "process.cpu.utilization", "gauge": {"dataPoints": [
                {"asDouble": 0.99, "timeUnixNano": "1762740000000000000", "attributes": [{"key": "pid", "value": {"stringValue": "42"}}]},
                {"flags": 1, "timeUnixNano": "1762740005000000000", "attributes": [{"key": "pid", "value": {"stringValue": "42"}}]}]}},
              {"name": "latency", "histogram": {"aggregationTemporality": 2, "dataPoints": [
                {"flags": 1, "timeUnixNano": "1762740005000000000", "explicitBounds": [0.1, 1.0]}]}}]}]}]});
        let (rows, _) = otlp_metrics(Some("application/json"), body.to_string().as_bytes(), "t", "r").expect("decode");
        let stale: Vec<bool> = rows.points.iter().map(|p| p.is_stale).collect();
        assert_eq!(stale[..2], [false, true], "the flagged gauge point is a staleness marker");
        // histogram: le=0.1, le=1, le=+Inf, _count, _sum
        assert_eq!(rows.points.len(), 2 + 5);
        assert!(rows.points[2..].iter().all(|p| p.is_stale && p.value.is_nan()));
    }

    #[test]
    fn otlp_json_logs_and_redaction() {
        let body = serde_json::json!({"resourceLogs": [{"resource": {"attributes": [{"key": "service.name", "value": {"stringValue": "api"}}]},
            "scopeLogs": [{"logRecords": [{"timeUnixNano": "1762740000000000000", "severityNumber": 17, "body": {"stringValue": "boom 42"},
              "attributes": [{"key": "http.authorization", "value": {"stringValue": "Bearer x"}}],
              "traceId": "5b8efff798038103d269b633813fc60c", "spanId": "eee19b7ec3c1b174"}]}]}]});
        let rows = otlp_logs(Some("application/json"), body.to_string().as_bytes()).expect("decode");
        let l = &rows.logs[0];
        assert_eq!(l.service_name, "api");
        assert_eq!(l.severity_text, "ERROR");
        assert_eq!(l.log_attrs["http.authorization"], "<redacted>");
        assert!(l.trace_id.is_some());
        assert_eq!(l.pattern_hash, pattern_hash("boom 7"));
    }

    #[test]
    fn loki_json() {
        let body =
            br#"{"streams":[{"stream":{"service_name":"web","level":"warn"},"values":[["1762740000000000000","slow request 120ms"]]}]}"#;
        let rows = loki_push(body).expect("decode");
        assert_eq!(rows.logs[0].severity_number, 13);
        assert_eq!(rows.logs[0].service_name, "web");
    }
}
