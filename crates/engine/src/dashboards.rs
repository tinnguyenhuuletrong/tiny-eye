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
    /// Keep this panel's computed points and extend them as data arrives, so a dashboard read
    /// only computes what is not stored (see `materialize.rs`). A data property, not presentation.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub materialize: bool,
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

/// `(dashboard id/query id, effective query)` of every panel marked `materialize: true`.
pub fn materialized(dashboards: &[Dashboard]) -> Vec<(String, serde_json::Value)> {
    dashboards
        .iter()
        .flat_map(|d| {
            d.panels.iter().filter(|p| p.materialize).map(move |p| {
                let q = p.effective_query(&d.vars);
                let qid = q.get("id").and_then(|v| v.as_str()).unwrap_or("panel").to_string();
                (format!("{}/{qid}", d.id), q)
            })
        })
        .collect()
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
                Ok(doc) => {
                    if p.materialize
                        && let Some(why) = tqs::materialize_blocker(&doc)
                    {
                        push("materialize", format!("panels[{i}].materialize"), format!("this panel cannot be materialized: {why}"));
                    }
                    doc.validate(now_ms).err().unwrap_or_default()
                }
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

/// Why a save was refused.
#[derive(Debug)]
pub enum SaveError {
    /// The state does not validate, or its file name is taken by another file: every problem.
    Invalid(Vec<Problem>),
    /// Writing the file failed.
    Io(anyhow::Error),
}

/// Validate and write `d` to `dir`: into the file that already holds this id, or `<id>.yaml`
/// (refused if that name holds another dashboard or does not parse). The write is atomic and
/// durable: a uniquely named temp file, fsync, rename, fsync of the directory. Callers that save
/// concurrently must serialise (the server holds its dashboards write lock). Returns the path.
pub fn save(dir: &str, d: &Dashboard) -> Result<PathBuf, SaveError> {
    let problems = d.validate(crate::node::now_ms());
    if !problems.is_empty() {
        return Err(SaveError::Invalid(problems));
    }
    let dir = Path::new(dir);
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display())).map_err(SaveError::Io)?;
    let mut existing = None;
    for p in yaml_files(dir) {
        let Ok(text) = std::fs::read_to_string(&p) else { continue };
        if serde_norway::from_str::<Dashboard>(&text).is_ok_and(|x| x.id == d.id) {
            existing = Some((p, text));
            break;
        }
    }
    // Keep the file's own leading comment block (its `$schema` line, notes for readers); a new
    // file gets a short header.
    let (path, header) = match existing {
        Some((p, text)) => {
            let h: String = text.lines().take_while(|l| l.starts_with('#')).map(|l| format!("{l}\n")).collect();
            (p, h)
        }
        None => {
            let p = dir.join(format!("{}.yaml", d.id));
            if p.exists() {
                return Err(SaveError::Invalid(vec![Problem {
                    code: "id",
                    path: "id".into(),
                    message: format!("{} exists but holds another dashboard or does not parse; pick another id", p.display()),
                }]));
            }
            (p, new_header(&d.title))
        }
    };
    let body = serde_norway::to_string(d).context("serialising the dashboard").map_err(SaveError::Io)?;
    let text = format!("{header}{body}");
    // Safety net: never replace a file with one that would not load back as this dashboard.
    if serde_norway::from_str::<Dashboard>(&text).ok().as_ref() != Some(d) {
        return Err(SaveError::Io(anyhow::anyhow!("the YAML written for {:?} would not load back as the same dashboard", d.id)));
    }
    write_atomic(dir, &path, &text).with_context(|| format!("writing {}", path.display())).map_err(SaveError::Io)?;
    Ok(path)
}

/// The comment header of a new file. The title is the only user text in it: every control and
/// line-separator character becomes a space, so it cannot end the comment line.
fn new_header(title: &str) -> String {
    let title: String = title.chars().map(|c| if c.is_control() || c == '\u{2028}' || c == '\u{2029}' { ' ' } else { c }).collect();
    format!(
        "# Dashboard \"{title}\": saved by tiny-eye. This file is the source of truth; commit it.\n\
         # Each panel: `ui` (display, unit, w of 12 columns, h rows) and `query` (TQS).\n"
    )
}

fn write_atomic(dir: &Path, path: &Path, text: &str) -> Result<()> {
    use std::io::Write;
    let tmp = dir.join(format!(".{}.{}.tmp", path.file_name().and_then(|n| n.to_str()).unwrap_or("dashboard"), ulid::Ulid::generate()));
    let written = (|| -> Result<()> {
        let mut f = std::fs::File::create_new(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written?;
    // Make the rename itself durable (best effort: not every platform can open a directory).
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn materialize_is_refused_where_points_depend_on_the_range() {
        let mut d = dash();
        d.panels[0].materialize = true;
        // kind: table is an instant query: not materializable.
        let now = 1_762_740_000_000;
        let ps = d.validate(now);
        assert!(ps.iter().any(|p| p.code == "materialize" && p.path == "panels[0].materialize"), "{ps:?}");
        d.panels[0].query["kind"] = "timeseries".into();
        assert!(d.validate(now).is_empty(), "{:?}", d.validate(now));
        let m = materialized(std::slice::from_ref(&d));
        assert_eq!(m[0].0, "shop/requests");
        assert_eq!(m[0].1["vars"]["service"], "shop-api", "dashboard vars are part of the registered query");
        // Round trip: the flag is written only when set.
        let y = serde_norway::to_string(&d).expect("yaml");
        assert!(y.contains("materialize: true"));
        d.panels[0].materialize = false;
        assert!(!serde_norway::to_string(&d).expect("yaml").contains("materialize"));
    }

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
        let Err(SaveError::Invalid(ps)) = save(d, &bad) else { panic!("invalid") };
        let codes: Vec<(String, &str)> = ps.into_iter().map(|p| (p.path, p.code)).collect();
        assert!(codes.contains(&("id".into(), "id")), "{codes:?}");
        assert!(codes.contains(&("panels[0].ui.w".into(), "schema")), "{codes:?}");
        assert!(codes.iter().any(|(p, c)| p.starts_with("panels[0].query.") && *c == "window"), "{codes:?}");
        assert!(yaml_files(dir.path()).is_empty(), "nothing written");
    }

    #[test]
    fn a_title_cannot_break_out_of_the_header_comment() {
        let dir = tempfile::tempdir().expect("tmp");
        let d = dir.path().to_str().expect("utf8");
        for (i, sep) in ["\n", "\r", "\r\n", "\u{85}", "\u{2028}", "\u{2029}", "\t\u{1b}"].iter().enumerate() {
            let mut x = dash();
            x.id = format!("t{i}");
            x.title = format!("evil{sep}id: other{sep}");
            let path = save(d, &x).expect("valid");
            let text = std::fs::read_to_string(&path).expect("read");
            let back: Dashboard = serde_norway::from_str(&text).expect("the file parses");
            assert_eq!(back, x, "{sep:?}: the title is stored as data, not as YAML");
            let header: Vec<&str> = text.lines().take_while(|l| l.starts_with('#')).collect();
            assert_eq!(header.len(), 2, "{sep:?}: {text}");
        }
        assert_eq!(load(d).expect("a restart loads every file").len(), 7);
    }

    #[test]
    fn a_new_id_never_overwrites_another_file() {
        let dir = tempfile::tempdir().expect("tmp");
        let d = dir.path().to_str().expect("utf8");
        let mut other = dash();
        other.id = "other".into();
        save(d, &other).expect("valid");
        std::fs::rename(dir.path().join("other.yaml"), dir.path().join("shop.yaml")).expect("mv");
        std::fs::write(dir.path().join("broken.yaml"), "id: [").expect("write");
        let before = std::fs::read_to_string(dir.path().join("shop.yaml")).expect("read");
        for (id, file) in [("shop", "shop.yaml"), ("broken", "broken.yaml")] {
            let mut x = dash();
            x.id = id.into();
            let Err(SaveError::Invalid(ps)) = save(d, &x) else { panic!("{id}: must be refused") };
            assert_eq!((ps[0].code, ps[0].path.as_str()), ("id", "id"), "{id}");
            assert!(ps[0].message.contains(file), "{}", ps[0].message);
        }
        assert_eq!(std::fs::read_to_string(dir.path().join("shop.yaml")).expect("read"), before, "untouched");
        assert_eq!(std::fs::read_dir(dir.path()).expect("ls").count(), 2, "no temp files left");
    }

    #[test]
    fn query_keys_keep_their_order() {
        let dir = tempfile::tempdir().expect("tmp");
        let d = dir.path().to_str().expect("utf8");
        let text = std::fs::read_to_string(save(d, &dash()).expect("valid")).expect("read");
        let at = |k: &str| text.find(k).unwrap_or_else(|| panic!("{k}"));
        // As written in the fixture, not alphabetical (serde_json `preserve_order`).
        assert!(at("tqs:") < at("id: requests") && at("id: requests") < at("kind:") && at("kind:") < at("time:"), "{text}");
        assert!(at("name: r") < at("metric:") && at("metric:") < at("match:") && at("match:") < at("fn:"), "{text}");
    }
}
