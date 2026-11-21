//! Stored tables, generated from `schema/tables.yaml` by build.rs (Arrow schema, typed rows,
//! builders and DuckLake DDL from one definition — SPEC §4.3).

#![allow(clippy::all)]

use arrow::array::{
    Array, ArrayRef, BinaryBuilder, BooleanBuilder, Float64Builder, Int64Builder, ListBuilder, MapBuilder, RecordBatch, StringBuilder,
    TimestampNanosecondBuilder, UInt8Builder, UInt64Builder,
};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit};
use std::sync::Arc;

/// Label / attribute map. Sorted, so its canonical form is its iteration order.
pub type Labels = std::collections::BTreeMap<String, String>;

fn map_type() -> DataType {
    MapBuilder::new(None, StringBuilder::new(), StringBuilder::new()).finish().data_type().clone()
}

#[allow(dead_code)]
fn list_type() -> DataType {
    ListBuilder::new(StringBuilder::new()).finish().data_type().clone()
}

include!(concat!(env!("OUT_DIR"), "/tables.rs"));
