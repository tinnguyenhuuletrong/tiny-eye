//! The TQS JSON Schema in api/openapi.yaml (published to agents, plugins and docs) and the Rust
//! model/validator in crates/tqs must agree. Every example in api/examples and every dashboard
//! panel must pass both; documents with shape errors must fail both.

#![allow(clippy::unwrap_used, clippy::type_complexity)]

use serde_json::{Value, json};

fn root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn tqs_schema() -> jsonschema::Validator {
    let spec: Value = serde_norway::from_str(server::api::OPENAPI_YAML).unwrap();
    let schema = json!({"$ref": "#/components/schemas/TqsDocument", "components": spec["components"]});
    jsonschema::validator_for(&schema).unwrap()
}

fn examples() -> Vec<(String, Value)> {
    let mut out = vec![];
    for e in std::fs::read_dir(root().join("api/examples")).unwrap() {
        let p = e.unwrap().path();
        out.push((p.display().to_string(), serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap()));
    }
    for d in engine::dashboards::load(root().join("dashboards").to_str().unwrap()).unwrap() {
        for (i, p) in d.panels.iter().enumerate() {
            // What runs: the stored query with the dashboard's vars as defaults.
            out.push((format!("dashboard {} panel {i}", d.id), p.effective_query(&d.vars)));
        }
    }
    assert!(out.len() >= 7);
    out
}

#[test]
fn examples_pass_schema_and_validator() {
    let schema = tqs_schema();
    for (name, doc) in examples() {
        let errs: Vec<String> = schema.iter_errors(&doc).map(|e| format!("{} at {}", e, e.instance_path())).collect();
        assert!(errs.is_empty(), "{name}: JSON Schema refuses: {errs:?}");
        let parsed = tqs::Tqs::parse(&doc.to_string()).unwrap_or_else(|p| panic!("{name}: Rust model refuses: {}", p.message));
        parsed.validate(engine::node::now_ms()).unwrap_or_else(|p| panic!("{name}: validator refuses: {p:?}"));
    }
}

#[test]
fn shape_errors_fail_both() {
    let schema = tqs_schema();
    let base = json!({"tqs": "3", "id": "x", "kind": "timeseries", "time": {"from": "-1h"},
        "queries": [{"name": "a", "metric": "m", "fn": {"op": "rate", "window": "5m"}}]});
    let mutations: Vec<(&str, Box<dyn Fn(&mut Value)>)> = vec![
        ("unknown top-level field", Box::new(|d| d["nope"] = json!(1))),
        ("unknown query field", Box::new(|d| d["queries"][0]["nope"] = json!(1))),
        ("unknown fn op", Box::new(|d| d["queries"][0]["fn"]["op"] = json!("predict_linear"))),
        ("unknown kind", Box::new(|d| d["kind"] = json!("heatmap"))),
        ("wrong type for window", Box::new(|d| d["queries"][0]["fn"]["window"] = json!(5))),
        ("unknown agg op", Box::new(|d| d["queries"][0]["agg"] = json!({"op": "median"}))),
        ("unknown time field", Box::new(|d| d["time"]["tz"] = json!("UTC"))),
        ("missing kind", Box::new(|d| _ = d.as_object_mut().unwrap().remove("kind"))),
    ];
    for (what, f) in mutations {
        let mut d = base.clone();
        f(&mut d);
        assert!(!schema.is_valid(&d), "{what}: JSON Schema accepts it");
        let rust = tqs::Tqs::parse(&d.to_string()).map_err(|p| vec![p]).and_then(|t| t.validate(0).map(|_| ()));
        assert!(rust.is_err(), "{what}: Rust model accepts it");
    }
}
