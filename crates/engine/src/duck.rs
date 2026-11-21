//! DuckDB connections: pinned extensions (D12), the DuckLake catalog, S3 secrets.

use anyhow::{Context, Result};
use duckdb::{Config, Connection};

#[derive(Clone, Debug, Default)]
pub struct S3Secret {
    pub endpoint: String,
    pub region: String,
    pub key_id: String,
    pub secret: String,
    pub use_ssl: bool,
}

#[derive(Clone, Debug)]
pub struct DuckSettings {
    /// Directory laid out as `<dir>/<duckdb version>/<platform>/*.duckdb_extension`; empty =
    /// DuckDB's default directory with auto-install.
    pub extension_dir: String,
    pub threads: usize,
    pub memory_limit: String,
}

impl Default for DuckSettings {
    fn default() -> Self {
        Self { extension_dir: String::new(), threads: 4, memory_limit: "2GB".into() }
    }
}

/// An in-memory DuckDB database with icu (DuckLake needs TIMESTAMPTZ math) + ducklake + httpfs.
pub fn open(s: &DuckSettings) -> Result<Connection> {
    let mut cfg = Config::default().threads(s.threads as i64)?.max_memory(&s.memory_limit)?;
    if !s.extension_dir.is_empty() {
        cfg = cfg
            .with("extension_directory", &s.extension_dir)?
            .with("autoinstall_known_extensions", "false")?
            .with("autoload_known_extensions", "false")?;
    }
    let conn = Connection::open_in_memory_with_flags(cfg)?;
    for ext in ["icu", "ducklake", "httpfs"] {
        if s.extension_dir.is_empty() {
            conn.execute_batch(&format!("INSTALL {ext}")).with_context(|| format!("INSTALL {ext}"))?;
        }
        conn.execute_batch(&format!("LOAD {ext}")).with_context(|| format!("LOAD {ext} from {:?} (run `make tools`)", s.extension_dir))?;
    }
    conn.execute_batch("SET enable_external_file_cache = true; SET parquet_metadata_cache = true;")?;
    Ok(conn)
}

pub fn add_s3_secret(conn: &Connection, s: &S3Secret) -> Result<()> {
    // Values are config, not query input; quote-escape anyway.
    let q = |v: &str| v.replace('\'', "''");
    let endpoint = s.endpoint.trim_start_matches("http://").trim_start_matches("https://");
    let ep = if endpoint.is_empty() { String::new() } else { format!(", ENDPOINT '{}', URL_STYLE 'path'", q(endpoint)) };
    conn.execute_batch(&format!(
        "CREATE OR REPLACE SECRET tiny_eye_s3 (TYPE s3, KEY_ID '{}', SECRET '{}', REGION '{}', USE_SSL {}{ep})",
        q(&s.key_id),
        q(&s.secret),
        q(&s.region),
        s.use_ssl
    ))?;
    Ok(())
}

/// Attach the DuckLake catalog as `lake`. `catalog` is a DuckDB file path or `postgres:<conninfo>`.
pub fn attach_lake(conn: &Connection, catalog: &str, data_path: &str) -> Result<()> {
    if catalog.starts_with("postgres:") {
        conn.execute_batch("LOAD postgres_scanner").context("LOAD postgres_scanner")?;
    }
    let q = |v: &str| v.replace('\'', "''");
    conn.execute_batch(&format!("ATTACH IF NOT EXISTS 'ducklake:{}' AS lake (DATA_PATH '{}')", q(catalog), q(data_path)))
        .with_context(|| format!("attaching DuckLake catalog {catalog}"))?;
    Ok(())
}

pub fn version(conn: &Connection) -> Result<String> {
    Ok(conn.query_row("SELECT version()", [], |r| r.get(0))?)
}
