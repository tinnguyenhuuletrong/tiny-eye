//! Configuration. `config/default.yaml` is the only place defaults live: it is embedded here
//! as the base layer, a user file is merged over it, then `TINY_EYE__A__B=v` env vars set
//! single keys. Unknown keys are refused so a typo never silently becomes a default.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer};
use serde_norway::Value;
use std::time::Duration;

pub const DEFAULT_YAML: &str = include_str!("../../../config/default.yaml");
const ENV_PREFIX: &str = "TINY_EYE__";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub node: Node,
    pub server: Server,
    pub storage: Storage,
    pub catalog: Catalog,
    pub duckdb: DuckDb,
    pub ingest: Ingest,
    pub coordinator: Coordinator,
    pub query: Query,
    pub auth: Auth,
    pub dashboards: Dashboards,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Node {
    pub id: String,
    pub region: String,
    pub tenant: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    pub listen: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Storage {
    pub url: String,
    pub s3: S3,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct S3 {
    pub endpoint: String,
    pub region: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub allow_http: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Catalog {
    pub url: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DuckDb {
    pub extension_dir: String,
    pub threads: usize,
    pub memory_limit: String,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Durability {
    /// Ack after arena admission; a crash loses the unflushed batch.
    Memory,
    /// Ack after a group-committed fsync to the local WAL; replayed on restart.
    Wal,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ingest {
    pub durability: Durability,
    pub wal_dir: String,
    pub flush_bytes: ByteSize,
    pub flush_interval: Dur,
    pub max_sample_age: Dur,
    pub max_sample_future: Dur,
    pub arena_limit: ByteSize,
    pub row_group_rows: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Coordinator {
    pub checkpoint_interval: Dur,
    pub hot_grace: Dur,
    pub compaction_interval: Dur,
    pub compaction_delay: Dur,
    pub retention: Dur,
    pub retention_interval: Dur,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    pub timeout: Dur,
    pub workers: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Auth {
    pub tokens: Vec<TokenSpec>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenSpec {
    /// Name of the env var holding the token (tokens never live in the file).
    pub token_env: String,
    pub scopes: Vec<Scope>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Ingest,
    Query,
    Admin,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dashboards {
    pub dir: String,
}

/// `64MB`, `1GB`, `512KB`, `100B` (powers of 1024).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteSize(pub u64);

impl<'de> Deserialize<'de> for ByteSize {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        parse_bytes(&s).map(ByteSize).ok_or_else(|| serde::de::Error::custom(format!("bad size {s:?} (use B, KB, MB, GB)")))
    }
}

pub fn parse_bytes(s: &str) -> Option<u64> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit())?;
    let (n, unit) = s.split_at(split);
    let n: u64 = n.parse().ok()?;
    let mult = match unit.trim() {
        "B" => 1,
        "KB" => 1 << 10,
        "MB" => 1 << 20,
        "GB" => 1 << 30,
        _ => return None,
    };
    Some(n * mult)
}

/// `250ms`, `5s`, `10m`, `1h`, `15d`, `1w` — same grammar as TQS durations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dur(pub Duration);

impl Dur {
    pub fn ms(&self) -> i64 {
        self.0.as_millis() as i64
    }
}

impl<'de> Deserialize<'de> for Dur {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        tqs::parse_duration_ms(&s)
            .map(|ms| Dur(Duration::from_millis(ms as u64)))
            .ok_or_else(|| serde::de::Error::custom(format!("bad duration {s:?} (use ms, s, m, h, d, w)")))
    }
}

fn merge(base: &mut Value, over: Value) {
    match (base, over) {
        (Value::Mapping(b), Value::Mapping(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(slot) => merge(slot, v),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (slot, v) => *slot = v,
    }
}

impl Config {
    /// Defaults ← optional file ← `TINY_EYE__*` environment.
    pub fn load(file: Option<&std::path::Path>) -> Result<Config> {
        let env: Vec<(String, String)> = std::env::vars().filter(|(k, _)| k.starts_with(ENV_PREFIX)).collect();
        let text = match file {
            Some(p) => Some(std::fs::read_to_string(p).with_context(|| format!("reading config {}", p.display()))?),
            None => None,
        };
        Self::from_layers(text.as_deref(), &env)
    }

    pub fn from_layers(file: Option<&str>, env: &[(String, String)]) -> Result<Config> {
        let mut v: Value = serde_norway::from_str(DEFAULT_YAML).context("embedded config/default.yaml")?;
        if let Some(text) = file {
            let over: Value = serde_norway::from_str(text).context("parsing config file")?;
            if !over.is_null() {
                merge(&mut v, over);
            }
        }
        for (k, val) in env {
            let path: Vec<String> = k.trim_start_matches(ENV_PREFIX).split("__").map(|p| p.to_lowercase()).collect();
            let mut slot = &mut v;
            for (i, part) in path.iter().enumerate() {
                let Value::Mapping(m) = slot else { bail!("env {k}: {} is not a section", path[..i].join(".")) };
                let key = Value::String(part.clone());
                if !m.contains_key(&key) {
                    bail!("env {k}: unknown key {}", path[..=i].join("."));
                }
                slot = m.get_mut(&key).expect("checked");
            }
            // Parse as YAML so numbers/bools/lists keep their type.
            *slot = serde_norway::from_str(val).unwrap_or(Value::String(val.clone()));
        }
        let cfg: Config = serde_norway::from_value(v).context("invalid configuration")?;
        cfg.check()?;
        Ok(cfg)
    }

    fn check(&self) -> Result<()> {
        if self.query.workers == 0 || self.duckdb.threads == 0 {
            bail!("query.workers and duckdb.threads must be > 0");
        }
        if self.ingest.flush_bytes.0 == 0 || self.ingest.row_group_rows == 0 {
            bail!("ingest.flush_bytes and ingest.row_group_rows must be > 0");
        }
        Ok(())
    }

    /// The effective config as YAML with secrets redacted (printed at start, SPEC §13).
    pub fn redacted_yaml(&self) -> String {
        let mut c = self.clone();
        for s in [&mut c.storage.s3.access_key_id, &mut c.storage.s3.secret_access_key] {
            if !s.is_empty() {
                *s = "<redacted>".into();
            }
        }
        format!("{c:#?}")
    }

    pub fn is_s3(&self) -> bool {
        self.storage.url.starts_with("s3://")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_parse() {
        let c = Config::from_layers(None, &[]).expect("defaults");
        assert_eq!(c.ingest.flush_bytes.0, 64 << 20);
        assert_eq!(c.ingest.flush_interval.0, Duration::from_secs(60));
        assert_eq!(c.ingest.durability, Durability::Wal);
    }

    #[test]
    fn file_then_env_override() {
        let file = "ingest:\n  flush_interval: 5s\nnode:\n  id: n7\n";
        let env = vec![("TINY_EYE__INGEST__FLUSH_INTERVAL".to_string(), "2s".to_string()), ("TINY_EYE__QUERY__WORKERS".into(), "8".into())];
        let c = Config::from_layers(Some(file), &env).expect("layers");
        assert_eq!(c.ingest.flush_interval.0, Duration::from_secs(2));
        assert_eq!(c.node.id, "n7");
        assert_eq!(c.query.workers, 8);
        assert_eq!(c.node.region, "local", "untouched keys keep defaults");
    }

    #[test]
    fn typos_are_refused() {
        assert!(Config::from_layers(Some("ingest:\n  flush_intervall: 5s\n"), &[]).is_err());
        assert!(Config::from_layers(None, &[("TINY_EYE__INGEST__NOPE".into(), "1".into())]).is_err());
        assert!(Config::from_layers(Some("ingest:\n  flush_bytes: 64XB\n"), &[]).is_err());
    }
}
