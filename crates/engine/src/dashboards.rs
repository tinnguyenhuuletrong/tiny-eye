//! Dashboards live in git as YAML files (Q12 default) and are loaded at start. Every panel is a
//! TQS document; a panel that does not validate stops the load with its file and problems.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Panel {
    #[serde(default)]
    pub display: Option<String>,
    #[serde(default)]
    pub unit: Option<String>,
    /// A TQS 3 document (kept as JSON so the API returns exactly what the file says).
    pub query: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dashboard {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub vars: BTreeMap<String, String>,
    pub panels: Vec<Panel>,
}

/// Load `*.yaml` / `*.yml` from `dir` (missing dir = no dashboards). Dashboard `vars` are the
/// defaults merged into each panel's `vars`.
pub fn load(dir: &str) -> Result<Vec<Dashboard>> {
    let mut out = vec![];
    let Ok(rd) = std::fs::read_dir(dir) else { return Ok(out) };
    let mut paths: Vec<_> =
        rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|x| x == "yaml" || x == "yml")).collect();
    paths.sort();
    for p in paths {
        let mut d: Dashboard =
            serde_norway::from_str(&std::fs::read_to_string(&p)?).with_context(|| format!("parsing dashboard {}", p.display()))?;
        for (i, panel) in d.panels.iter_mut().enumerate() {
            if let Some(obj) = panel.query.as_object_mut() {
                let vars = obj.entry("vars").or_insert_with(|| serde_json::json!({}));
                if let Some(v) = vars.as_object_mut() {
                    for (k, val) in &d.vars {
                        v.entry(k.clone()).or_insert_with(|| serde_json::Value::String(val.clone()));
                    }
                }
                if vars.as_object().is_some_and(|v| v.is_empty()) {
                    obj.remove("vars");
                }
            }
            let doc =
                tqs::Tqs::parse(&panel.query.to_string()).map_err(|e| anyhow::anyhow!("{}: panel {i}: {}", p.display(), e.message))?;
            if let Err(problems) = doc.validate(crate::node::now_ms()) {
                bail!("{}: panel {i} ({}) does not validate: {problems:?}", p.display(), doc.id);
            }
        }
        out.push(d);
    }
    Ok(out)
}
