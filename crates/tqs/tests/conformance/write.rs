//! The ingest write path: rows -> sorted Arrow batch -> ZSTD Parquet -> PUT, then a
//! commit envelope written with If-None-Match. No DuckDB on this path.

use crate::synth::{Sample, Span};
pub const TENANT: &str = "default";
use anyhow::Result;
use arrow::array::{
    Array, ArrayRef, FixedSizeBinaryBuilder, Float64Array, Int64Array, MapBuilder, RecordBatch, StringArray, StringBuilder,
    TimestampNanosecondArray, UInt8Array, UInt64Array,
};
use arrow::datatypes::{DataType, Field, Schema, TimeUnit};
use bytes::Bytes;
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, Encoding, ZstdLevel};
use parquet::file::properties::{EnabledStatistics, WriterProperties};
use parquet::schema::types::ColumnPath;
use std::sync::Arc;

/// Length-prefixed canonical encoding so no label value can collide with a separator.
pub fn series_id(region: &str, metric: &str, canonical_labels: &str) -> u64 {
    let mut buf = Vec::new();
    for part in [TENANT, region, metric, canonical_labels] {
        buf.extend_from_slice(&(part.len() as u32).to_le_bytes());
        buf.extend_from_slice(part.as_bytes());
    }
    xxhash_rust::xxh64::xxh64(&buf, 0)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SpanLayout {
    /// Concept layout: sorted by (trace_id, start), SBBF bloom filter on trace_id.
    TraceSortedBloom,
    /// Same sort, no bloom filter: isolates what the bloom filter buys.
    TraceSortedNoBloom,
    /// v5 spec layout: sorted by (service_name, trace_id, start), bloom on trace_id.
    ServiceFirstBloom,
}

pub fn span_batch(spans: &mut [Span], layout: SpanLayout) -> Result<RecordBatch> {
    match layout {
        SpanLayout::ServiceFirstBloom => {
            spans.sort_by(|a, b| (a.service, a.trace_id, a.start_ns).cmp(&(b.service, b.trace_id, b.start_ns)))
        }
        _ => spans.sort_by_key(|a| (a.trace_id, a.start_ns)),
    }
    let mut trace_ids = FixedSizeBinaryBuilder::with_capacity(spans.len(), 16);
    let mut attrs = MapBuilder::new(None, StringBuilder::new(), StringBuilder::new());
    for s in spans.iter() {
        trace_ids.append_value(s.trace_id)?;
        attrs.keys().append_value("http.route");
        attrs.values().append_value(s.http_route);
        attrs.append(true)?;
    }
    let attrs = attrs.finish();
    let schema = Arc::new(Schema::new(vec![
        Field::new("tenant", DataType::Utf8, false),
        Field::new("region", DataType::Utf8, false),
        Field::new("trace_id", DataType::FixedSizeBinary(16), false),
        Field::new("span_id", DataType::UInt64, false),
        Field::new("parent_span_id", DataType::UInt64, true),
        Field::new("start_time_ns", DataType::Timestamp(TimeUnit::Nanosecond, None), false),
        Field::new("duration_ns", DataType::UInt64, false),
        Field::new("service_name", DataType::Utf8, false),
        Field::new("span_name", DataType::Utf8, false),
        Field::new("status_code", DataType::UInt8, false),
        Field::new("span_attrs", attrs.data_type().clone(), false),
    ]));
    let cols: Vec<ArrayRef> = vec![
        Arc::new(StringArray::from_iter_values(spans.iter().map(|_| TENANT))),
        Arc::new(StringArray::from_iter_values(spans.iter().map(|s| s.region))),
        Arc::new(trace_ids.finish()),
        Arc::new(UInt64Array::from_iter_values(spans.iter().map(|s| s.span_id))),
        Arc::new(UInt64Array::from_iter(spans.iter().map(|s| s.parent_span_id))),
        Arc::new(TimestampNanosecondArray::from_iter_values(spans.iter().map(|s| s.start_ns))),
        Arc::new(UInt64Array::from_iter_values(spans.iter().map(|s| s.duration_ns))),
        Arc::new(StringArray::from_iter_values(spans.iter().map(|s| s.service))),
        Arc::new(StringArray::from_iter_values(spans.iter().map(|s| s.name))),
        Arc::new(UInt8Array::from_iter_values(spans.iter().map(|s| s.status))),
        Arc::new(attrs),
    ];
    Ok(RecordBatch::try_new(schema, cols)?)
}

/// Raw points carry `value` only. `positive_delta` stays NULL until the compactor, the
/// single writer for a partition, computes it (concept decision D2).
pub fn point_batch(samples: &mut [Sample]) -> Result<RecordBatch> {
    samples.sort_by_key(|s| (s.series_id, s.ts_ms));
    let schema = Arc::new(Schema::new(vec![
        Field::new("tenant", DataType::Utf8, false),
        Field::new("region", DataType::Utf8, false),
        Field::new("metric_name", DataType::Utf8, false),
        Field::new("series_id", DataType::UInt64, false),
        Field::new("timestamp_ms", DataType::Int64, false),
        Field::new("value", DataType::Float64, false),
        Field::new("positive_delta", DataType::Float64, true),
    ]));
    let cols: Vec<ArrayRef> = vec![
        Arc::new(StringArray::from_iter_values(samples.iter().map(|_| TENANT))),
        Arc::new(StringArray::from_iter_values(samples.iter().map(|s| s.region))),
        Arc::new(StringArray::from_iter_values(samples.iter().map(|_| "http_requests_total"))),
        Arc::new(UInt64Array::from_iter_values(samples.iter().map(|s| s.series_id))),
        Arc::new(Int64Array::from_iter_values(samples.iter().map(|s| s.ts_ms))),
        Arc::new(Float64Array::from_iter_values(samples.iter().map(|s| s.value))),
        Arc::new(Float64Array::from(vec![None; samples.len()])),
    ];
    Ok(RecordBatch::try_new(schema, cols)?)
}

pub fn parquet_bytes(batch: &RecordBatch, bloom_on: Option<&str>, row_group_rows: usize) -> Result<Bytes> {
    let col = |c: &str| ColumnPath::from(c);
    let mut props = WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::try_new(3)?))
        .set_max_row_group_row_count(Some(row_group_rows))
        .set_statistics_enabled(EnabledStatistics::Page);
    if batch.schema().index_of("timestamp_ms").is_ok() {
        props = props
            .set_column_dictionary_enabled(col("timestamp_ms"), false)
            .set_column_encoding(col("timestamp_ms"), Encoding::DELTA_BINARY_PACKED)
            .set_column_dictionary_enabled(col("value"), false)
            .set_column_encoding(col("value"), Encoding::BYTE_STREAM_SPLIT);
    }
    if let Some(c) = bloom_on {
        props = props
            .set_column_dictionary_enabled(col(c), false)
            .set_column_bloom_filter_enabled(col(c), true)
            .set_column_bloom_filter_fpp(col(c), 0.01);
    }
    let mut buf = Vec::new();
    let mut writer = ArrowWriter::try_new(&mut buf, batch.schema(), Some(props.build()))?;
    writer.write(batch)?;
    writer.close()?;
    Ok(Bytes::from(buf))
}
