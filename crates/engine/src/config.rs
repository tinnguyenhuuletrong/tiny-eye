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
    /// Nodes sharing `storage.url`: publish this node's catalog copy, query the others' copies.
    pub fleet: Fleet,
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
    /// Node id, unique among the nodes sharing `storage.url`. With `region` it names the node's
    /// object prefix `<region>/<id>/`; empty falls back to the default.
    pub id: String,
    /// Region label stamped on rows (one bucket per region, SPEC §12); first part of the node's
    /// object prefix. Empty falls back to the default.
    pub region: String,
    /// Tenant stamped on rows (single tenant in v1).
    pub tenant: String,
}

impl Node {
    /// This node's own prefix in object storage: `<region>/<id>`.
    pub fn prefix(&self) -> String {
        format!("{}/{}", self.region, self.id)
    }
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
    /// A DuckDB file path, or `postgres:<libpq conninfo>` (then both fleet intervals must be `0s`:
    /// a fleet publishes and reads catalog files).
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
    /// A catalog snapshot is expired once a newer one has existed this long; only then are the
    /// files no newer snapshot references deleted. At least `fleet.publish_interval + 3 ×
    /// fleet.refresh_interval + query.timeout`, so a catalog copy another node may still be
    /// reading never names a deleted file (readers drop a copy older than the smaller of their own
    /// and the publisher's grace minus twice their refresh interval and their query timeout).
    pub snapshot_grace: Dur,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Fleet {
    /// Publish a copy of this node's catalog to `_fleet/` in object storage this often; `0s` = never.
    pub publish_interval: Interval,
    /// Look for the catalog copies of other nodes sharing `storage.url` this often and query their
    /// data with this node's; `0s` = never. A refresh gives up the listing and downloads not done
    /// within this time. Copies too old to be safe are skipped (see `coordinator.snapshot_grace`).
    pub refresh_interval: Interval,
    /// Local directory for downloaded catalog copies: they live in `<dir>/<node.id>/` (this node's
    /// id), which is emptied at start.
    pub dir: String,
    /// A catalog copy of another node that unpacks to more than this is refused (copies come from
    /// shared storage and are not trusted).
    pub max_copy_bytes: ByteSize,
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
    /// A step is stored once it is this old. A sample that arrives later than this for a stored
    /// step makes the next tick recompute the stored steps from its timestamp on, so stored points
    /// never miss accepted data; a larger value stores later and recomputes less often.
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

    /// In the config grammar, as written in a file (`30s`, `10m`).
    pub fn text(&self) -> String {
        serde_json::to_value(self).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_else(|| format!("{}ms", self.ms()))
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
    /// Positive, like TQS durations: `0s` is refused (only an [`Interval`] may be off).
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        tqs::parse_duration_ms(&s)
            .map(|ms| Dur(Duration::from_millis(ms as u64)))
            .ok_or_else(|| serde::de::Error::custom(format!("bad duration {s:?} (use ms, s, m, h, d, w; greater than 0)")))
    }
}

/// A [`Dur`] that may also be `0s`, meaning "never" (the fleet intervals).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interval(pub Duration);

impl Interval {
    pub fn is_off(&self) -> bool {
        self.0.is_zero()
    }

    pub fn text(&self) -> String {
        if self.is_off() { "0s".into() } else { Dur(self.0).text() }
    }
}

impl JsonSchema for Interval {
    fn schema_name() -> Cow<'static, str> {
        "Interval".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type": "string", "pattern": "^[0-9]+(ms|s|m|h|d|w)$", "description": "Duration: ms, s, m, h, d, w, e.g. 30s; 0s = never", "examples": ["30s", "0s"]})
    }
}

impl Serialize for Interval {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.text())
    }
}

impl<'de> Deserialize<'de> for Interval {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if s.strip_prefix('0').is_some_and(|u| ["ms", "s", "m", "h", "d", "w"].contains(&u)) {
            return Ok(Interval(Duration::ZERO));
        }
        tqs::parse_duration_ms(&s)
            .map(|ms| Interval(Duration::from_millis(ms as u64)))
            .ok_or_else(|| serde::de::Error::custom(format!("bad interval {s:?} (use ms, s, m, h, d, w; 0s = never)")))
    }
}

/// Region and id name an object storage prefix (`<region>/<id>/`) and a manifest
/// (`_fleet/<id>.json`): one path segment, never `_fleet`.
pub fn check_id(key: &str, v: &str) -> Result<()> {
    if v.is_empty()
        || v == "."
        || v == ".."
        || v == "_fleet"
        || !v.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        bail!("{key} {v:?} names an object storage prefix: use letters, digits, '.', '_' and '-' only (not '.', '..' or '_fleet')");
    }
    Ok(())
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
        let defaults: Value = serde_norway::from_str(DEFAULT_YAML).context("embedded config/default.yaml")?;
        let mut v = defaults.clone();
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
            *slot = if val.trim().is_empty() {
                Value::String(val.clone())
            } else {
                serde_norway::from_str(val).unwrap_or(Value::String(val.clone()))
            };
        }
        // Region and id name the node's object prefix: empty or null means "not set", never `//`.
        for key in ["id", "region"] {
            let unset = |x: Option<&Value>| x.is_none_or(|x| x.is_null() || x.as_str().is_some_and(|s| s.trim().is_empty()));
            if unset(v.get("node").and_then(|n| n.get(key)))
                && let (Some(d), Some(Value::Mapping(node))) = (defaults.get("node").and_then(|n| n.get(key)), v.get_mut("node"))
            {
                node.insert(Value::String(key.into()), d.clone());
            }
        }
        let cfg: Config = serde_norway::from_value(v).context("invalid configuration")?;
        cfg.check()?;
        Ok(cfg)
    }

    fn check(&self) -> Result<()> {
        check_id("node.id", &self.node.id)?;
        check_id("node.region", &self.node.region)?;
        let f = &self.fleet;
        if self.catalog.url.starts_with("postgres:") && !(f.publish_interval.is_off() && f.refresh_interval.is_off()) {
            bail!(
                "catalog.url is a postgres: catalog, but a fleet publishes and reads DuckDB catalog files: set fleet.publish_interval \
                 and fleet.refresh_interval to 0s (a fleet needs the file catalog)"
            );
        }
        // A reader drops a copy older than grace - 2 × refresh_interval - query.timeout; this keeps
        // that cutoff at least publish_interval + refresh_interval (a copy is at most one publish
        // interval old when listed, and a refresh takes at most one refresh interval), so a live
        // publisher is never dropped.
        let need = f.publish_interval.0 + 3 * f.refresh_interval.0 + self.query.timeout.0;
        if self.coordinator.snapshot_grace.0 < need {
            bail!(
                "coordinator.snapshot_grace ({}) must be at least fleet.publish_interval + 3 × fleet.refresh_interval + query.timeout \
                 ({} + 3 × {} + {} = {}): another node may still be reading a catalog copy that names the files older snapshots referenced",
                self.coordinator.snapshot_grace.text(),
                f.publish_interval.text(),
                f.refresh_interval.text(),
                self.query.timeout.text(),
                Dur(need).text()
            );
        }
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

    /// Settings that work but are likely not what was meant; logged at start.
    pub fn warnings(&self) -> Vec<String> {
        let mut out = vec![];
        let (i, c, f) = (&self.ingest, &self.coordinator, &self.fleet);
        // Rows of another node reach this one at most this late; a materialized step stored before
        // they arrive is recomputed when they do (see fleet.rs), which costs a recompute each time.
        let lag = i.flush_interval.0 + c.checkpoint_interval.0 + f.publish_interval.0 + f.refresh_interval.0;
        if !f.refresh_interval.is_off() && self.materialize.settle.0 < lag {
            out.push(format!(
                "materialize.settle ({}) is shorter than the worst-case lag of another node's rows (ingest.flush_interval + \
                 coordinator.checkpoint_interval + fleet.publish_interval + fleet.refresh_interval = {}): materialized panels \
                 store steps before every node's rows for them arrived and recompute them when the rows show up",
                self.materialize.settle.text(),
                Dur(lag).text()
            ));
        }
        out
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
        let c = Config::from_layers(
            Some("catalog: { url: \"postgres:host=db user=te password=s3cret dbname=lake\" }\nfleet: { publish_interval: 0s, refresh_interval: 0s }\n"),
            &[],
        )
        .expect("cfg");
        let y = c.redacted_yaml();
        assert!(!y.contains("s3cret") && y.contains("password=<redacted>"), "{y}");
    }

    #[test]
    fn empty_region_or_id_fall_back_to_the_defaults() {
        for file in ["node: { id: '', region: '' }\n", "node:\n  id:\n  region:\n"] {
            let c = Config::from_layers(Some(file), &[]).expect("empty is not an error");
            assert_eq!((c.node.id.as_str(), c.node.region.as_str()), ("node-0", "local"), "{file}");
        }
        let env = [("TINY_EYE__NODE__ID".to_string(), "".to_string()), ("TINY_EYE__NODE__REGION".into(), " ".into())];
        let c = Config::from_layers(None, &env).expect("empty env values are not an error");
        assert_eq!((c.node.id.as_str(), c.node.region.as_str()), ("node-0", "local"));
        let c = Config::from_layers(Some("node: { id: n-1, region: eu-west.2 }\n"), &[]).expect("set values are kept");
        assert_eq!((c.node.id.as_str(), c.node.region.as_str()), ("n-1", "eu-west.2"));
        for bad in [
            "node: { id: a/b }",
            "node: { region: '..' }",
            "node: { region: 'eu west' }",
            "node: { id: _fleet }",
            "node: { region: _fleet }",
        ] {
            assert!(Config::from_layers(Some(bad), &[]).is_err(), "{bad}: not a single path segment");
        }
    }

    #[test]
    fn snapshot_grace_covers_publish_three_refreshes_and_query_timeout() {
        let e = Config::from_layers(Some("coordinator: { snapshot_grace: 149s }\n"), &[]).expect_err("30s + 3 × 30s + 30s > 149s");
        assert!(
            format!("{e:#}").contains(
                "coordinator.snapshot_grace (149s) must be at least fleet.publish_interval + 3 × fleet.refresh_interval + query.timeout \
                 (30s + 3 × 30s + 30s = 150s)"
            ),
            "{e:#}"
        );
        Config::from_layers(Some("coordinator: { snapshot_grace: 150s }\n"), &[]).expect("exactly the sum is enough");
        let off = "fleet: { publish_interval: 0s, refresh_interval: 0s }\ncoordinator: { snapshot_grace: ";
        Config::from_layers(Some(&format!("{off}30s }}\n")), &[]).expect("fleet off: only the query timeout counts");
        assert!(Config::from_layers(Some(&format!("{off}29s }}\n")), &[]).is_err());
        let reader = "fleet: { publish_interval: 0s, refresh_interval: 10s }\ncoordinator: { snapshot_grace: ";
        Config::from_layers(Some(&format!("{reader}60s }}\n")), &[]).expect("a node that only reads: 3 × 10s + 30s");
        assert!(Config::from_layers(Some(&format!("{reader}59s }}\n")), &[]).is_err());
    }

    #[test]
    fn zero_is_refused_for_every_duration_but_the_fleet_intervals() {
        for bad in [
            "coordinator: { retention: 0s }",
            "materialize: { resolution: 0s }",
            "query: { timeout: 0s }",
            "ingest: { flush_interval: 0ms }",
            "coordinator: { checkpoint_interval: 0m }",
            "coordinator: { snapshot_grace: 0s }",
            "materialize: { settle: 0s }",
        ] {
            let e = Config::from_layers(Some(bad), &[]).expect_err(bad);
            assert!(format!("{e:#}").contains("greater than 0"), "{bad}: {e:#}");
        }
        let c =
            Config::from_layers(Some("fleet: { publish_interval: 0s, refresh_interval: 0ms }"), &[]).expect("fleet intervals may be off");
        assert!(c.fleet.publish_interval.is_off() && c.fleet.refresh_interval.is_off());
        let back = Config::from_layers(Some(&c.redacted_yaml()), &[]).expect("printed back as 0s");
        assert!(back.fleet.publish_interval.is_off());
    }

    #[test]
    fn a_postgres_catalog_needs_the_fleet_off() {
        let pg = "catalog: { url: 'postgres:host=db dbname=lake' }\n";
        let e = Config::from_layers(Some(pg), &[]).expect_err("fleet on by default");
        assert!(format!("{e:#}").contains("set fleet.publish_interval and fleet.refresh_interval to 0s"), "{e:#}");
        assert!(Config::from_layers(Some(&format!("{pg}fleet: {{ publish_interval: 0s }}\n")), &[]).is_err(), "refresh still on");
        Config::from_layers(Some(&format!("{pg}fleet: {{ publish_interval: 0s, refresh_interval: 0s }}\n")), &[]).expect("fleet off");
    }

    #[test]
    fn short_materialize_settle_warns_when_reading_other_nodes() {
        assert!(Config::from_layers(None, &[]).expect("defaults").warnings().is_empty(), "defaults: 60s + 5s + 30s + 30s < 5m");
        let c = Config::from_layers(Some("materialize: { settle: 2m }\n"), &[]).expect("valid");
        assert!(c.warnings().iter().any(|w| w.contains("materialize.settle (2m)")), "{:?}", c.warnings());
        let alone = Config::from_layers(Some("materialize: { settle: 2m }\nfleet: { refresh_interval: 0s }\n"), &[]).expect("valid");
        assert!(alone.warnings().is_empty(), "no other node is read");
    }

    #[test]
    fn typos_are_refused() {
        assert!(Config::from_layers(Some("ingest:\n  flush_intervall: 5s\n"), &[]).is_err());
        assert!(Config::from_layers(None, &[("TINY_EYE__INGEST__NOPE".into(), "1".into())]).is_err());
        assert!(Config::from_layers(Some("ingest:\n  flush_bytes: 64XB\n"), &[]).is_err());
    }
}
