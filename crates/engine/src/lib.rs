//! tiny-eye engine: everything between a decoded sample and a query result.

pub mod config;
pub mod dashboards;
pub mod duck;
pub mod fleet;
pub mod ids;
pub mod materialize;
pub mod metrics;
pub mod node;
pub mod parquet;
pub mod query;
pub mod store;
pub mod tables;
pub mod wal;

pub use config::Config;
pub use node::{IngestError, Node, Rows};
