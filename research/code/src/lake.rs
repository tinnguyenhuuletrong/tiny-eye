//! Environment wiring: the object stores (one bucket per region) and DuckDB
//! connections attached to the DuckLake catalog in PostgreSQL.

use anyhow::{Context, Result, bail};
use duckdb::{Config, Connection};
use object_store::aws::{AmazonS3, AmazonS3Builder, S3ConditionalPut};
use std::sync::Arc;

pub const REGIONS: [&str; 2] = ["us-east", "eu-west"];
pub const TENANT: &str = "default";

#[derive(Clone)]
pub struct Env {
    pub s3_endpoint: String,
    pub s3_key: String,
    pub s3_secret: String,
    pub pg: String,
    pub ext_dir: String,
}

impl Env {
    pub fn from_process() -> Result<Self> {
        let var = |k: &str| {
            std::env::var(k).with_context(|| format!("{k} is not set: run `eval \"$(scripts/stack.sh env)\"` first"))
        };
        Ok(Self {
            s3_endpoint: var("POC_S3_ENDPOINT")?,
            s3_key: var("POC_S3_KEY")?,
            s3_secret: var("POC_S3_SECRET")?,
            pg: var("POC_PG")?,
            ext_dir: var("POC_DUCKDB_EXT")?,
        })
    }

    pub fn bucket(region: &str) -> String {
        format!("telemetry-{region}")
    }

    pub fn store(&self, region: &str) -> Result<Arc<AmazonS3>> {
        let store = AmazonS3Builder::new()
            .with_endpoint(&self.s3_endpoint)
            .with_bucket_name(Self::bucket(region))
            .with_access_key_id(&self.s3_key)
            .with_secret_access_key(&self.s3_secret)
            .with_region("us-east-1")
            .with_allow_http(true)
            .with_virtual_hosted_style_request(false)
            // If-None-Match / If-Match: the commit protocol and the coordinator lease depend on them.
            .with_conditional_put(S3ConditionalPut::ETagMatch)
            .build()
            .with_context(|| format!("building S3 client for {}", Self::bucket(region)))?;
        Ok(Arc::new(store))
    }

    /// A DuckDB connection with httpfs + DuckLake loaded from the local extension
    /// directory (the public extension host is not reachable from every machine).
    pub fn duckdb(&self) -> Result<Connection> {
        let cfg = Config::default()
            .with("extension_directory", &self.ext_dir)?
            .with("autoinstall_known_extensions", "false")?
            .with("autoload_known_extensions", "false")?;
        let conn = Connection::open_in_memory_with_flags(cfg)?;
        for ext in ["httpfs", "ducklake", "postgres_scanner"] {
            conn.execute_batch(&format!("LOAD {ext}"))
                .with_context(|| format!("LOAD {ext}: is {}/v2.0.0/linux_amd64/{ext}.duckdb_extension present?", self.ext_dir))?;
        }
        let endpoint = self.s3_endpoint.trim_start_matches("http://");
        conn.execute_batch(&format!(
            "CREATE SECRET s3 (TYPE s3, KEY_ID '{}', SECRET '{}', ENDPOINT '{endpoint}', URL_STYLE 'path', USE_SSL false, REGION 'us-east-1');
             SET enable_external_file_cache = false; SET parquet_metadata_cache = false;",
            self.s3_key, self.s3_secret
        ))?;
        Ok(conn)
    }

    /// Connection attached to the shared catalog. DATA_PATH is the us-east bucket; files
    /// from other regions are registered by absolute path (one catalog, many buckets).
    pub fn lake(&self) -> Result<Connection> {
        let conn = self.duckdb()?;
        conn.execute_batch(&format!(
            "ATTACH 'ducklake:postgres:{}' AS lake (DATA_PATH 's3://{}/lake/'); USE lake;",
            self.pg,
            Self::bucket("us-east")
        ))
        .context("attaching DuckLake catalog in PostgreSQL")?;
        Ok(conn)
    }
}

pub fn schema_ddl() -> &'static str {
    // Types mirror what the Rust writer emits so `ducklake_add_data_files` maps columns by name.
    "CREATE TABLE IF NOT EXISTS spans (
        tenant VARCHAR, region VARCHAR, trace_id BLOB, span_id UBIGINT, parent_span_id UBIGINT,
        start_time_ns TIMESTAMP_NS, duration_ns UBIGINT, service_name VARCHAR, span_name VARCHAR,
        status_code UTINYINT, span_attrs MAP(VARCHAR, VARCHAR));
     CREATE TABLE IF NOT EXISTS spans_nobloom AS SELECT * FROM spans WITH NO DATA;
     CREATE TABLE IF NOT EXISTS spans_v5 AS SELECT * FROM spans WITH NO DATA;
     CREATE TABLE IF NOT EXISTS metrics_points (
        tenant VARCHAR, region VARCHAR, metric_name VARCHAR, series_id UBIGINT,
        timestamp_ms BIGINT, value DOUBLE, positive_delta DOUBLE);
     CREATE TABLE IF NOT EXISTS applied_envelopes (
        envelope_id VARCHAR, region VARCHAR, node_id VARCHAR, node_epoch BIGINT,
        seq_to BIGINT, files INTEGER);
     CREATE TABLE IF NOT EXISTS span_hist_partials (
        region VARCHAR, partial_key VARCHAR, service_name VARCHAR, bucket SMALLINT, n UBIGINT);"
}

/// Count and bytes of S3 GETs DuckDB issued since the log was last truncated.
pub fn http_gets(conn: &Connection) -> Result<(u64, u64)> {
    let (n, bytes): (i64, Option<i64>) = conn.query_row(
        "SELECT count(*), sum(regexp_extract(message, 'bytes=(\\d+)-(\\d+)', 2)::BIGINT
                             - regexp_extract(message, 'bytes=(\\d+)-(\\d+)', 1)::BIGINT + 1)
         FROM duckdb_logs WHERE type = 'HTTP' AND message LIKE '%type'': GET%'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if n < 0 {
        bail!("negative request count from duckdb_logs");
    }
    Ok((n as u64, bytes.unwrap_or(0) as u64))
}
