//! schema/generated and docs/reference/mcp-tools.md are generated (`make schemas`); this fails when a committed file is stale, so the
//! editor grammars, MCP tool docs and config schema can never drift from their sources.

#![allow(clippy::unwrap_used)]

#[test]
fn doc_schema_is_up_to_date() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut stale = vec![];
    for (name, body) in server::schemas::generate() {
        if std::fs::read_to_string(dir.join(name)).ok().as_deref() != Some(body.as_str()) {
            stale.push(name);
        }
    }
    assert!(stale.is_empty(), "stale generated files: {stale:?} — run `make schemas` and commit");
}

#[test]
fn config_files_match_the_config_schema() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let schema = jsonschema::validator_for(&engine::config::json_schema()).unwrap();
    for f in ["config/default.yaml", "config/dev.yaml"] {
        let v: serde_json::Value = serde_norway::from_str(&std::fs::read_to_string(root.join(f)).unwrap()).unwrap();
        let errs: Vec<String> = schema.iter_errors(&v).map(|e| format!("{e} at {}", e.instance_path())).collect();
        assert!(errs.is_empty(), "{f}: {errs:?}");
    }
    let bad: serde_json::Value = serde_norway::from_str("ingest:\n  flush_intervall: 5s\n").unwrap();
    assert!(!schema.is_valid(&bad), "typos are refused by the schema too");
}

#[test]
fn dashboards_match_the_dashboard_schema() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let schema: serde_json::Value = serde_json::from_str(server::api::DASHBOARD_SCHEMA_JSON).unwrap();
    let v = jsonschema::validator_for(&schema).unwrap();
    for e in std::fs::read_dir(root.join("dashboards")).unwrap() {
        let p = e.unwrap().path();
        let d: serde_json::Value = serde_norway::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        let errs: Vec<String> = v.iter_errors(&d).map(|e| format!("{e} at {}", e.instance_path())).collect();
        assert!(errs.is_empty(), "{}: {errs:?}", p.display());
    }
}
