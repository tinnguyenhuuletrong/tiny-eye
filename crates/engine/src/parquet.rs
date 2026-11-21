//! Parquet layout (SPEC §4.2): rows sorted by the table's sort key, ZSTD 3, page statistics,
//! delta / byte-stream-split encodings and split-block bloom filters where tables.yaml asks.

use crate::tables::{SCHEMA_VERSION, Table};
use anyhow::Result;
use arrow::array::RecordBatch;
use arrow::compute::{SortColumn, lexsort_to_indices, take_record_batch};
use bytes::Bytes;
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, Encoding, ZstdLevel};
use parquet::file::metadata::KeyValue;
use parquet::file::properties::{EnabledStatistics, WriterProperties};
use parquet::schema::types::ColumnPath;

/// Sort a batch by the table's sort columns (stable, nulls first).
pub fn sort(table: Table, batch: &RecordBatch) -> Result<RecordBatch> {
    if batch.num_rows() < 2 {
        return Ok(batch.clone());
    }
    let cols: Vec<SortColumn> = table
        .sort_columns()
        .iter()
        .map(|c| Ok(SortColumn { values: batch.column(batch.schema().index_of(c)?).clone(), options: None }))
        .collect::<Result<_>>()?;
    let idx = lexsort_to_indices(&cols, None)?;
    Ok(take_record_batch(batch, &idx)?)
}

pub fn writer_properties(table: Table, row_group_rows: usize) -> Result<WriterProperties> {
    let col = |c: &str| ColumnPath::from(c);
    let mut p = WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::try_new(3)?))
        .set_max_row_group_row_count(Some(row_group_rows))
        .set_statistics_enabled(EnabledStatistics::Page)
        .set_key_value_metadata(Some(vec![KeyValue::new("schema_version".into(), SCHEMA_VERSION.to_string())]));
    for c in table.delta_columns() {
        p = p.set_column_dictionary_enabled(col(c), false).set_column_encoding(col(c), Encoding::DELTA_BINARY_PACKED);
    }
    for c in table.byte_stream_split_columns() {
        p = p.set_column_dictionary_enabled(col(c), false).set_column_encoding(col(c), Encoding::BYTE_STREAM_SPLIT);
    }
    for c in table.bloom_columns() {
        p = p
            .set_column_dictionary_enabled(col(c), false)
            .set_column_bloom_filter_enabled(col(c), true)
            .set_column_bloom_filter_fpp(col(c), 0.01);
    }
    Ok(p.build())
}

/// Sorted batches → one Parquet file in memory. The Arrow schema is taken from the batches
/// (the generated builders, or DuckDB's export of the hot table — same columns, same names);
/// encodings and bloom filters come from tables.yaml by column name.
pub fn write(table: Table, batches: &[RecordBatch], row_group_rows: usize) -> Result<Bytes> {
    let schema = batches.first().map(|b| b.schema()).unwrap_or_else(|| table.schema());
    let mut buf = Vec::new();
    let mut w = ArrowWriter::try_new(&mut buf, schema, Some(writer_properties(table, row_group_rows)?))?;
    for b in batches {
        w.write(b)?;
    }
    w.close()?;
    Ok(Bytes::from(buf))
}
