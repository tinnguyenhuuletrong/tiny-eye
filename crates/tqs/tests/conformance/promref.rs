//! Prometheus as the reference implementation: the fixtures are imported into a TSDB
//! with `promtool tsdb create-blocks-from openmetrics`, a Prometheus serves them, and each
//! test case's PromQL result is compared point by point with the TQS result.

use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub struct Prom {
    child: Child,
    base: String,
    agent: ureq::Agent,
    pub version: String,
}

/// (canonical labels without __name__, t_ms) → value, as Prometheus returned it.
pub type Points = BTreeMap<(String, i64), f64>;

pub fn canonical(metric: &serde_json::Map<String, serde_json::Value>) -> String {
    let mut kv: Vec<String> =
        metric.iter().filter(|(k, _)| k.as_str() != "__name__").map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or_default())).collect();
    kv.sort();
    kv.join(",")
}

impl Prom {
    pub fn start(tools: &Path, work: &Path, openmetrics: &str) -> Result<Prom> {
        let _ = std::fs::remove_dir_all(work);
        std::fs::create_dir_all(work)?;
        let om = work.join("fixtures.om");
        std::fs::write(&om, openmetrics)?;
        let data: PathBuf = work.join("data");
        let out =
            Command::new(tools.join("promtool")).args(["tsdb", "create-blocks-from", "openmetrics"]).arg(&om).arg(&data).output().context(
                "running promtool: build it with `GOBIN=.tools go install github.com/prometheus/prometheus/cmd/promtool@latest`",
            )?;
        if !out.status.success() {
            bail!("promtool import failed: {}", String::from_utf8_lossy(&out.stderr));
        }
        std::fs::write(work.join("prometheus.yml"), "global:\n  evaluation_interval: 1m\n")?;
        let log = std::fs::File::create(work.join("prometheus.log"))?;
        let child = Command::new(tools.join("prometheus"))
            .arg(format!("--config.file={}", work.join("prometheus.yml").display()))
            .arg(format!("--storage.tsdb.path={}", data.display()))
            .args(["--storage.tsdb.retention.time=100y", "--web.listen-address=127.0.0.1:19090", "--query.lookback-delta=5m"])
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .context("starting prometheus")?;
        let agent: ureq::Agent = ureq::Agent::config_builder().proxy(None).build().into();
        let base = "http://127.0.0.1:19090".to_string();
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if agent.get(format!("{base}/-/ready")).call().is_ok() {
                break;
            }
            if Instant::now() > deadline {
                bail!("prometheus not ready after 60 s; see {}", work.join("prometheus.log").display());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        let mut p = Prom { child, base, agent, version: String::new() };
        let info: serde_json::Value = p.get("/api/v1/status/buildinfo", &[])?;
        p.version = info["data"]["version"].as_str().unwrap_or("unknown").to_string();
        Ok(p)
    }

    fn get(&self, path: &str, query: &[(&str, String)]) -> Result<serde_json::Value> {
        let mut req = self.agent.get(format!("{}{path}", self.base));
        for (k, v) in query {
            req = req.query(*k, v);
        }
        let mut resp = req.call().with_context(|| format!("GET {path} {query:?}"))?;
        let body = resp.body_mut().read_to_string()?;
        let v: serde_json::Value = serde_json::from_str(&body)?;
        if v["status"] != "success" {
            bail!("prometheus error for {query:?}: {body}");
        }
        Ok(v)
    }

    pub fn range(&self, promql: &str, start_ms: i64, end_ms: i64, step_ms: i64) -> Result<Points> {
        let v = self.get(
            "/api/v1/query_range",
            &[
                ("query", promql.to_string()),
                ("start", format!("{:.3}", start_ms as f64 / 1000.0)),
                ("end", format!("{:.3}", end_ms as f64 / 1000.0)),
                ("step", format!("{}", step_ms as f64 / 1000.0)),
            ],
        )?;
        let mut out = Points::new();
        for s in v["data"]["result"].as_array().into_iter().flatten() {
            let key = canonical(s["metric"].as_object().unwrap_or(&serde_json::Map::new()));
            for pair in s["values"].as_array().into_iter().flatten() {
                let t = (pair[0].as_f64().unwrap_or(0.0) * 1000.0).round() as i64;
                let val: f64 = pair[1].as_str().unwrap_or("NaN").parse().unwrap_or(f64::NAN);
                out.insert((key.clone(), t), val);
            }
        }
        Ok(out)
    }

    pub fn instant(&self, promql: &str, t_ms: i64) -> Result<Points> {
        let v = self.get("/api/v1/query", &[("query", promql.to_string()), ("time", format!("{:.3}", t_ms as f64 / 1000.0))])?;
        let mut out = Points::new();
        let data = &v["data"];
        if data["resultType"] == "scalar" {
            let val: f64 = data["result"][1].as_str().unwrap_or("NaN").parse().unwrap_or(f64::NAN);
            out.insert((String::new(), t_ms), val);
            return Ok(out);
        }
        for s in data["result"].as_array().into_iter().flatten() {
            let key = canonical(s["metric"].as_object().unwrap_or(&serde_json::Map::new()));
            let val: f64 = s["value"][1].as_str().unwrap_or("NaN").parse().unwrap_or(f64::NAN);
            out.insert((key, t_ms), val);
        }
        Ok(out)
    }
}

impl Drop for Prom {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
