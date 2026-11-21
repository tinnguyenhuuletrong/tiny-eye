//! Dashboards are YAML files in git (Q12): the files are the source of truth. They are loaded at
//! start exactly as written; the UI displays them, edits them, and sends the whole new state back
//! (`PUT /api/v1/dashboards/{id}`), which is validated like a file and written to `<id>.yaml`.
//!
//! A panel is data plus presentation: `query` (a TQS document) and `ui` (how and where it is
//! drawn). Dashboard `vars` are defaults for every panel's query; they are applied when a panel
//! is validated or run, never written into the stored queries.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use tqs::Problem;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PanelUi {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// Columns of the 12-column grid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub w: Option<u8>,
    /// Rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub h: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Panel {
    /// Presentation first, as dashboards are written by hand: `ui` then `query`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui: Option<PanelUi>,
    /// A TQS 3 document, kept as JSON so the API returns exactly what the file says.
    pub query: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dashboard {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub vars: BTreeMap<String, String>,
    pub panels: Vec<Panel>,
}

const DISPLAYS: &[&str] = &["timeseries", "stat", "table", "bar", "traces", "logs"];
/// Also the file name, so it can never leave the dashboards directory.
static ID: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"^[a-z0-9][a-z0-9_-]{0,63}$").expect("static regex"));

impl Panel {
    /// The panel's query with the dashboard's `vars` as defaults (what is validated and run).
    pub fn effective_query(&self, vars: &BTreeMap<String, String>) -> serde_json::Value {
        let mut q = self.query.clone();
        if let Some(obj) = q.as_object_mut()
            && !vars.is_empty()
        {
            let v = obj.entry("vars").or_insert_with(|| serde_json::json!({}));
            if let Some(m) = v.as_object_mut() {
                for (k, val) in vars {
                    m.entry(k.clone()).or_insert_with(|| serde_json::Value::String(val.clone()));
                }
            }
        }
        q
    }
}

impl Dashboard {
    /// Every problem in the dashboard, with JSON paths (`panels[2].query.time.step`, …).
    pub fn validate(&self, now_ms: i64) -> Vec<Problem> {
        let mut out = vec![];
        let mut push = |code: &'static str, path: String, message: String| out.push(Problem { code, path, message });
        if !ID.is_match(&self.id) {
            push("id", "id".into(), "dashboard id must match ^[a-z0-9][a-z0-9_-]{0,63}$ (it is also the file name)".into());
        }
        if self.title.trim().is_empty() {
            push("schema", "title".into(), "title must not be empty".into());
        }
        for (i, p) in self.panels.iter().enumerate() {
            if let Some(ui) = &p.ui {
                if ui.display.as_deref().is_some_and(|d| !DISPLAYS.contains(&d)) {
                    push("schema", format!("panels[{i}].ui.display"), format!("display is one of {}", DISPLAYS.join(", ")));
                }
                if ui.w.is_some_and(|w| !(1..=12).contains(&w)) {
                    push("schema", format!("panels[{i}].ui.w"), "w is 1..=12 columns".into());
                }
                if ui.h.is_some_and(|h| !(1..=8).contains(&h)) {
                    push("schema", format!("panels[{i}].ui.h"), "h is 1..=8 rows".into());
                }
            }
            let problems = match tqs::Tqs::parse(&p.effective_query(&self.vars).to_string()) {
                Err(e) => vec![e],
                Ok(doc) => doc.validate(now_ms).err().unwrap_or_default(),
            };
            for e in problems {
                push(e.code, format!("panels[{i}].query.{}", e.path.trim_start_matches("$")), e.message);
            }
        }
        out
    }
}

fn yaml_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else { return vec![] };
    let mut paths: Vec<_> =
        rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|x| x == "yaml" || x == "yml")).collect();
    paths.sort();
    paths
}

/// Load every dashboard in `dir` (missing dir = none). A dashboard that does not validate stops
/// the load with its file and problems.
pub fn load(dir: &str) -> Result<Vec<Dashboard>> {
    let mut out: Vec<Dashboard> = vec![];
    for p in yaml_files(Path::new(dir)) {
        let d: Dashboard =
            serde_norway::from_str(&std::fs::read_to_string(&p)?).with_context(|| format!("parsing dashboard {}", p.display()))?;
        let problems = d.validate(crate::node::now_ms());
        if !problems.is_empty() {
            bail!("{} does not validate: {problems:?}", p.display());
        }
        if out.iter().any(|x| x.id == d.id) {
            bail!("{}: dashboard id {:?} is used by another file", p.display(), d.id);
        }
        out.push(d);
    }
    Ok(out)
}

/// Validate and write `d` to `dir`: into the file that already holds this id, or `<id>.yaml`.
/// The write is atomic (temp file + rename). Returns the path written.
pub fn save(dir: &str, d: &Dashboard) -> Result<PathBuf, Vec<Problem>> {
    let problems = d.validate(crate::node::now_ms());
    if !problems.is_empty() {
        return Err(problems);
    }
    let io = |e: std::io::Error| vec![Problem { code: "io", path: "$".into(), message: e.to_string() }];
    let dir = Path::new(dir);
    std::fs::create_dir_all(dir).map_err(io)?;
    let existing = yaml_files(dir)
        .into_iter()
        .find(|p| std::fs::read_to_string(p).ok().and_then(|t| serde_norway::from_str::<Dashboard>(&t).ok()).is_some_and(|x| x.id == d.id));
    let body = serde_norway::to_string(d).map_err(|e| vec![Problem { code: "io", path: "$".into(), message: e.to_string() }])?;
    // Keep the file's own leading comment block (its `$schema` line, notes for readers); a new
    // file gets a short header.
    let header = existing
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|t| t.lines().take_while(|l| l.starts_with('#')).map(|l| format!("{l}\n")).collect::<String>())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| {
            format!(
                "# Dashboard \"{}\": saved by tiny-eye. This file is the source of truth; commit it.\n\
                 # Each panel: `ui` (display, unit, w of 12 columns, h rows) and `query` (TQS).\n",
                d.title.replace('\n', " ")
            )
        });
    let path = existing.unwrap_or_else(|| dir.join(format!("{}.yaml", d.id)));
    let text = format!("{header}{body}");
    let tmp = path.with_extension("yaml.tmp");
    std::fs::write(&tmp, text).map_err(io)?;
    std::fs::rename(&tmp, &path).map_err(io)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dash() -> Dashboard {
        serde_norway::from_str(
            r#"
id: shop
title: Shop overview
vars: { service: shop-api }
panels:
  - ui: { display: stat, unit: req/s, w: 3, h: 2 }
    query:
      tqs: "3"
      id: requests
      kind: table
      time: { from: "-5m" }
      queries:
        - { name: r, metric: http_requests_total, match: [{ label: job, op: "=", value: "$service" }], fn: { op: rate, window: 2m }, agg: { op: sum } }
"#,
        )
        .expect("fixture")
    }

    #[test]
    fn save_then_load_round_trips_without_merging_vars() {
        let dir = tempfile::tempdir().expect("tmp");
        let d = dir.path().to_str().expect("utf8");
        let path = save(d, &dash()).expect("valid");
        assert_eq!(path.file_name().and_then(|n| n.to_str()), Some("shop.yaml"));
        let back = load(d).expect("load");
        assert_eq!(back, vec![dash()], "the file holds exactly the state that was saved");
        assert!(back[0].panels[0].query.get("vars").is_none(), "dashboard vars stay out of stored queries");
        // A second save with a new layout replaces the same file.
        let mut d2 = dash();
        d2.panels[0].ui.as_mut().expect("ui").w = Some(6);
        save(d, &d2).expect("valid");
        assert_eq!(load(d).expect("load")[0].panels[0].ui.as_ref().and_then(|u| u.w), Some(6));
        assert_eq!(yaml_files(dir.path()).len(), 1);
    }

    #[test]
    fn invalid_state_is_refused_with_paths() {
        let dir = tempfile::tempdir().expect("tmp");
        let d = dir.path().to_str().expect("utf8");
        let mut bad = dash();
        bad.id = "../etc".into();
        bad.panels[0].ui.as_mut().expect("ui").w = Some(13);
        bad.panels[0].query["queries"][0]["fn"]["window"] = serde_json::json!("soon");
        let codes: Vec<(String, &str)> = save(d, &bad).expect_err("invalid").into_iter().map(|p| (p.path, p.code)).collect();
        assert!(codes.contains(&("id".into(), "id")), "{codes:?}");
        assert!(codes.contains(&("panels[0].ui.w".into(), "schema")), "{codes:?}");
        assert!(codes.iter().any(|(p, c)| p.starts_with("panels[0].query.") && *c == "window"), "{codes:?}");
        assert!(yaml_files(dir.path()).is_empty(), "nothing written");
    }
}
