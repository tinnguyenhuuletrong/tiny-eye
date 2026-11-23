//! Configuration. `config/default.yaml` is the only place defaults live: it is embedded here
//! as the base layer, a user file is merged over it, then `TINY_EYE__A__B=v` env vars set
//! single keys. Unknown keys are refused so a typo never silently becomes a default.

use anyhow::{Context, Result, bail};
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_norway::Value;
use std::borrow::Cow;
use std::time::Duration;

pub const DEFAULT_YAML: &str = include_str!("../../../config/default.yaml");
const ENV_PREFIX: &str = "TINY_EYE__";

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
/// tiny-eye configuration file (merged over the built-in config/default.yaml).
pub struct Config {
    /// Identity of this node; tenant and region are stamped on every row it ingests.
    pub node: Node,
    /// HTTP listener (API, OTLP/HTTP, remote-write, Loki push, /metrics).
    pub server: Server,
    /// Object storage for Parquet data files and commit envelopes.
    pub storage: Storage,
    /// DuckLake catalog holding table metadata and file lists.
    pub catalog: Catalog,
    /// Embedded DuckDB settings.
    pub duckdb: DuckDb,
    /// Write path: durability, flush thresholds, admission limits (SPEC §5).
    pub ingest: Ingest,
    /// Checkpointer, compactor and retention (SPEC §6).
    pub coordinator: Coordinator,
    /// Query execution limits (SPEC §7).
    pub query: Query,
    /// Bearer-token auth; scopes per operation come from api/openapi.yaml (`x-scope`).
    pub auth: Auth,
    /// Dashboards loaded from YAML files at start.
    pub dashboards: Dashboards,
    /// Materialized dashboard panels (`materialize: true` on a panel).
    pub materialize: Materialize,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Node {
    /// Unique node id; part of every file name and envelope.
    pub id: String,
    /// Region label stamped on rows (one bucket per region, SPEC §12).
    pub region: String,
    /// Tenant stamped on rows (single tenant in v1).
    pub tenant: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Server {
    /// `host:port` to listen on.
    pub listen: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Storage {
    /// `./dir`, `file:///abs/dir` or `s3://bucket/prefix`.
    pub url: String,
    pub s3: S3,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct S3 {
    /// Empty for AWS; e.g. `http://127.0.0.1:19000` for MinIO.
    pub endpoint: String,
    pub region: String,
    /// Secret: prefer the env var TINY_EYE__STORAGE__S3__ACCESS_KEY_ID.
    pub access_key_id: String,
    /// Secret: prefer the env var TINY_EYE__STORAGE__S3__SECRET_ACCESS_KEY.
    pub secret_access_key: String,
    pub allow_http: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Catalog {
    /// A DuckDB file path (single node) or `postgres:<libpq conninfo>` (cluster).
    pub url: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DuckDb {
    /// `<dir>/<duckdb version>/<platform>/*.duckdb_extension`; empty = auto-install.
    pub extension_dir: String,
    pub threads: usize,
    pub memory_limit: String,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Durability {
    /// Ack after arena admission; a crash loses the unflushed batch.
    Memory,
    /// Ack after a group-committed fsync to the local WAL; replayed on restart.
    Wal,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Ingest {
    pub durability: Durability,
    /// Directory of the write-ahead log (durability: wal).
    pub wal_dir: String,
    /// Flush the open batch when it reaches this size...
    pub flush_bytes: ByteSize,
    /// ...or this age.
    pub flush_interval: Dur,
    /// Samples older than this are rejected.
    pub max_sample_age: Dur,
    /// Samples further ahead than this are rejected.
    pub max_sample_future: Dur,
    /// Above this many buffered bytes ingest answers 429 / RESOURCE_EXHAUSTED.
    pub arena_limit: ByteSize,
    /// Parquet row-group size.
    pub row_group_rows: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Coordinator {
    /// How often pending envelopes are applied to the catalog.
    pub checkpoint_interval: Dur,
    /// Keep committed rows in RAM this long after the catalog has them.
    pub hot_grace: Dur,
    /// How often closed hours are checked for compaction.
    pub compaction_interval: Dur,
    /// Compact an hour this long after it closes.
    pub compaction_delay: Dur,
    /// Data older than this is deleted.
    pub retention: Dur,
    /// How often retention runs.
    pub retention_interval: Dur,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Query {
    /// A query taking longer from arrival (waiting for a free connection included) is interrupted.
    pub timeout: Dur,
    /// DuckDB connections serving queries in parallel.
    pub workers: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Auth {
    pub tokens: Vec<TokenSpec>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TokenSpec {
    /// Name of the env var holding the token (tokens never live in the file).
    pub token_env: String,
    pub scopes: Vec<Scope>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Ingest,
    Query,
    Admin,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Dashboards {
    /// Directory of dashboard YAML files (schema: schema/generated/dashboard.schema.json).
    pub dir: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Materialize {
    /// Node-local DuckDB file holding materialized points; empty keeps them in memory (they are
    /// derived data: losing them only means computing the uncovered range again).
    pub path: String,
    /// Step of the stored points. A read whose step is a multiple of it uses them.
    pub resolution: Dur,
    /// A step is stored once it is this old. Samples that arrive later than this after their
    /// timestamp are not in the stored points (raw queries still see them).
    pub settle: Dur,
    /// At most this many panels are materialized; the rest are skipped with a warning. Bounds the
    /// store: `max_panels * max_points_per_panel * ~24 B`.
    pub max_panels: usize,
    /// Stored points kept per panel. Past it the oldest steps are dropped (a read computes them
    /// again), so a wide panel (many series per step) cannot grow the store without bound. A
    /// panel that cannot hold at least 240 steps (1 h at 15 s) is not stored at all.
    pub max_points_per_panel: u64,
    /// Points one background slice may produce (computed and written in one statement). The
    /// slice length adapts to the panel's width, so the working memory of a tick stays bounded.
    pub max_slice_points: u64,
}

/// `64MB`, `1GB`, `512KB`, `100B` (powers of 1024).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteSize(pub u64);

impl JsonSchema for ByteSize {
    fn schema_name() -> Cow<'static, str> {
        "ByteSize".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type": "string", "pattern": "^[0-9]+(B|KB|MB|GB)$", "description": "Size: B, KB, MB, GB (powers of 1024), e.g. 64MB", "examples": ["64MB", "1GB"]})
    }
}

impl Serialize for ByteSize {
    /// Back to the config grammar, in the largest exact unit (`64MB`, `1536KB`).
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let u = [("GB", 1u64 << 30), ("MB", 1 << 20), ("KB", 1 << 10)].into_iter().find(|(_, n)| self.0 > 0 && self.0.is_multiple_of(*n));
        s.serialize_str(&match u {
            Some((name, n)) => format!("{}{name}", self.0 / n),
            None => format!("{}B", self.0),
        })
    }
}

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

impl JsonSchema for Dur {
    fn schema_name() -> Cow<'static, str> {
        "Duration".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type": "string", "pattern": "^[0-9]+(ms|s|m|h|d|w)$", "description": "Duration: ms, s, m, h, d, w, e.g. 60s", "examples": ["5s", "10m", "15d"]})
    }
}

impl Serialize for Dur {
    /// Back to the config grammar, in the largest exact unit (`60s` → `1m`, `15d`).
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let ms = self.0.as_millis() as u64;
        let u = [("w", 604_800_000u64), ("d", 86_400_000), ("h", 3_600_000), ("m", 60_000), ("s", 1_000)]
            .into_iter()
            .find(|(_, n)| ms > 0 && ms.is_multiple_of(*n));
        s.serialize_str(&match u {
            Some((name, n)) => format!("{}{name}", ms / n),
            None => format!("{ms}ms"),
        })
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
        // `postgres:host=… password=…` or a postgres URL with user:password@.
        static PW: std::sync::LazyLock<regex::Regex> =
            std::sync::LazyLock::new(|| regex::Regex::new(r"(password=)\S+|(://[^:/@]+:)[^@]+(@)").expect("static regex"));
        c.catalog.url = PW.replace_all(&c.catalog.url, "${1}${2}<redacted>${3}").into_owned();
        serde_norway::to_string(&c).unwrap_or_else(|e| format!("<config not printable: {e}>"))
    }

    pub fn is_s3(&self) -> bool {
        self.storage.url.starts_with("s3://")
    }
}

/// JSON Schema of the config file (for editors), with every default from config/default.yaml
/// set as `default` so hover shows it. Written to schema/generated/config.schema.json.
pub fn json_schema() -> serde_json::Value {
    let mut schema = serde_json::to_value(schemars::schema_for!(Config)).expect("schema is JSON");
    let defaults: serde_json::Value = serde_norway::from_str(DEFAULT_YAML).expect("embedded defaults");
    fn inject(s: &mut serde_json::Value, d: &serde_json::Value, defs: &serde_json::Value) {
        let target = match s.get("$ref").and_then(|r| r.as_str()).and_then(|r| r.strip_prefix("#/$defs/")) {
            Some(name) => defs.get(name).cloned(),
            None => None,
        };
        if let Some(props) = s.get_mut("properties").and_then(|p| p.as_object_mut()) {
            for (k, ps) in props.iter_mut() {
                if let Some(dv) = d.get(k) {
                    if dv.is_object() {
                        inject(ps, dv, defs);
                    } else {
                        ps["default"] = dv.clone();
                    }
                }
            }
        } else if let Some(mut t) = target {
            // Inline the referenced section so each property can carry its own default.
            inject(&mut t, d, defs);
            if let Some(desc) = s.get("description").cloned() {
                t["description"] = desc;
            }
            *s = t;
        }
    }
    fn optional(v: &mut serde_json::Value) {
        match v {
            serde_json::Value::Object(m) => {
                m.remove("required");
                m.values_mut().for_each(optional);
            }
            serde_json::Value::Array(a) => a.iter_mut().for_each(optional),
            _ => {}
        }
    }
    let defs = schema.get("$defs").cloned().unwrap_or_default();
    inject(&mut schema, &defaults, &defs);
    // A user file is an overlay: every key has a default, so none is required.
    optional(&mut schema);
    schema
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
    fn printed_config_is_yaml_that_loads_back() {
        let c = Config::from_layers(None, &[]).expect("defaults");
        let back = Config::from_layers(Some(&c.redacted_yaml()), &[]).expect("the printed config is a valid config file");
        assert_eq!(back.ingest.flush_bytes, c.ingest.flush_bytes);
        assert_eq!(back.coordinator.retention, c.coordinator.retention);
    }

    #[test]
    fn catalog_passwords_are_redacted() {
        let c =
            Config::from_layers(Some("catalog: { url: \"postgres:host=db user=te password=s3cret dbname=lake\" }\n"), &[]).expect("cfg");
        let y = c.redacted_yaml();
        assert!(!y.contains("s3cret") && y.contains("password=<redacted>"), "{y}");
    }

    #[test]
    fn typos_are_refused() {
        assert!(Config::from_layers(Some("ingest:\n  flush_intervall: 5s\n"), &[]).is_err());
        assert!(Config::from_layers(None, &[("TINY_EYE__INGEST__NOPE".into(), "1".into())]).is_err());
        assert!(Config::from_layers(Some("ingest:\n  flush_bytes: 64XB\n"), &[]).is_err());
    }
}
