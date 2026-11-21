//! Generates `api.rs` from `api/openapi.yaml` (the source of truth for the HTTP API):
//!   - `router()`: one axum route per operation → `crate::handlers::<operationId>`;
//!   - `OPERATIONS`: method, path, operationId and required scope (`x-scope`) per route;
//!   - one serde type per `components.schemas` entry (except `x-codegen: false`);
//!   - `MCP_TOOLS`: operations with `x-mcp-tool`, input schema = request body with refs inlined.
//!
//! The DTO generator covers the subset the spec uses: objects with typed properties,
//! `additionalProperties` maps, arrays, `$ref`, string enums/consts, and `prefixItems` tuples.

#![allow(clippy::unwrap_used)] // a build script should fail loudly

use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt::Write as _;

fn camel(s: &str) -> String {
    s.split(['_', '-']).filter(|p| !p.is_empty()).map(|p| p[..1].to_uppercase() + &p[1..]).collect()
}

fn ref_name(r: &str) -> String {
    r.rsplit('/').next().expect("ref").to_string()
}

/// Schemas marked `x-codegen: false` (validated elsewhere): referenced as raw JSON.
static RAW: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();

/// Rust type for a schema.
fn rust_type(s: &Value) -> String {
    if let Some(r) = s.get("$ref").and_then(Value::as_str) {
        let n = ref_name(r);
        return if RAW.get().is_some_and(|raw| raw.contains(&n)) { "serde_json::Value".into() } else { n };
    }
    if let Some(items) = s.get("prefixItems").and_then(Value::as_array) {
        return format!("({})", items.iter().map(rust_type).collect::<Vec<_>>().join(", "));
    }
    match s.get("type").and_then(Value::as_str) {
        Some("string") => "String".into(),
        Some("boolean") => "bool".into(),
        Some("integer") => "i64".into(),
        Some("number") => "f64".into(),
        Some("array") => format!("Vec<{}>", s.get("items").map(rust_type).unwrap_or_else(|| "serde_json::Value".into())),
        Some("object") => match s.get("additionalProperties") {
            Some(ap) if ap.is_object() => format!("std::collections::BTreeMap<String, {}>", rust_type(ap)),
            _ if s.get("properties").is_none() => "serde_json::Map<String, serde_json::Value>".into(),
            _ => panic!("inline object schemas with properties need a name in components.schemas: {s}"),
        },
        _ => "serde_json::Value".into(),
    }
}

fn gen_type(name: &str, s: &Value, out: &mut String) {
    let doc = s.get("description").and_then(Value::as_str).unwrap_or("");
    for line in doc.lines() {
        writeln!(out, "/// {line}").unwrap();
    }
    match (s.get("type").and_then(Value::as_str), s.get("properties")) {
        (Some("object"), Some(props)) => {
            let required: Vec<&str> =
                s.get("required").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
            writeln!(out, "#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]").unwrap();
            if s.get("additionalProperties") == Some(&Value::Bool(false)) {
                writeln!(out, "#[serde(deny_unknown_fields)]").unwrap();
            }
            writeln!(out, "pub struct {name} {{").unwrap();
            for (field, fs) in props.as_object().expect("properties") {
                if let Some(d) = fs.get("description").and_then(Value::as_str) {
                    writeln!(out, "    /// {}", d.lines().next().unwrap_or("")).unwrap();
                }
                let ident = match field.as_str() {
                    "type" | "fn" | "match" | "ref" => format!("r#{field}"),
                    f => f.to_string(),
                };
                let ty = rust_type(fs);
                if required.contains(&field.as_str()) {
                    writeln!(out, "    pub {ident}: {ty},").unwrap();
                } else {
                    writeln!(out, "    #[serde(default, skip_serializing_if = \"Option::is_none\")]\n    pub {ident}: Option<{ty}>,")
                        .unwrap();
                }
            }
            writeln!(out, "}}\n").unwrap();
        }
        _ => writeln!(out, "pub type {name} = {};\n", rust_type(s)).unwrap(),
    }
}

/// Inline every `#/components/schemas/X` ref into `$defs` so a tool schema is self-contained.
fn self_contained(schema: &Value, all: &serde_json::Map<String, Value>) -> Value {
    fn collect(v: &Value, all: &serde_json::Map<String, Value>, defs: &mut BTreeMap<String, Value>) {
        match v {
            Value::Object(m) => {
                if let Some(r) = m.get("$ref").and_then(Value::as_str) {
                    let n = ref_name(r);
                    if !defs.contains_key(&n) {
                        let target = all.get(&n).unwrap_or_else(|| panic!("unknown ref {r}")).clone();
                        defs.insert(n.clone(), Value::Null);
                        collect(&target, all, defs);
                        defs.insert(n, target);
                    }
                }
                m.values().for_each(|x| collect(x, all, defs));
            }
            Value::Array(a) => a.iter().for_each(|x| collect(x, all, defs)),
            _ => {}
        }
    }
    fn rewrite(v: &mut Value) {
        match v {
            Value::Object(m) => {
                m.remove("x-codegen");
                if let Some(Value::String(r)) = m.get_mut("$ref") {
                    *r = r.replace("#/components/schemas/", "#/$defs/");
                }
                m.values_mut().for_each(rewrite);
            }
            Value::Array(a) => a.iter_mut().for_each(rewrite),
            _ => {}
        }
    }
    let mut defs = BTreeMap::new();
    collect(schema, all, &mut defs);
    // MCP wants `type: object` at the root: a root `$ref` is replaced by its target.
    let mut root = match schema.get("$ref").and_then(Value::as_str) {
        Some(r) => all[&ref_name(r)].clone(),
        None => schema.clone(),
    };
    if let Value::Object(m) = &mut root {
        m.insert("$defs".into(), Value::Object(defs.into_iter().collect()));
    }
    rewrite(&mut root);
    root
}

fn main() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../api/openapi.yaml");
    println!("cargo:rerun-if-changed={path}");
    let spec: Value =
        serde_norway::from_str(&std::fs::read_to_string(path).expect("read api/openapi.yaml")).expect("parse api/openapi.yaml");
    let schemas = spec["components"]["schemas"].as_object().expect("components.schemas").clone();
    RAW.set(schemas.iter().filter(|(_, s)| s.get("x-codegen") == Some(&Value::Bool(false))).map(|(n, _)| n.clone()).collect())
        .expect("set once");
    let mut o = String::from("// @generated by crates/server/build.rs from api/openapi.yaml — do not edit.\n\n");

    // Types
    o.push_str("pub mod types {\n    #![allow(clippy::all, dead_code)]\n");
    let mut body = String::new();
    for (name, s) in &schemas {
        if s.get("x-codegen") == Some(&Value::Bool(false)) {
            continue;
        }
        gen_type(name, s, &mut body);
    }
    for line in body.lines() {
        writeln!(o, "    {line}").unwrap();
    }
    o.push_str("}\n\n");

    // Operations, router, MCP tools
    let mut ops = vec![];
    let mut tools = vec![];
    for (p, item) in spec["paths"].as_object().expect("paths") {
        for (method, op) in item.as_object().expect("path item") {
            let id = op["operationId"].as_str().unwrap_or_else(|| panic!("{method} {p}: operationId missing"));
            let scope = op.get("x-scope").and_then(Value::as_str).unwrap_or_else(|| panic!("{id}: x-scope missing"));
            assert!(["public", "ingest", "query", "admin"].contains(&scope), "{id}: bad x-scope {scope}");
            ops.push((method.to_uppercase(), p.clone(), id.to_string(), scope.to_string()));
            if let Some(t) = op.get("x-mcp-tool") {
                let input = op
                    .pointer("/requestBody/content/application~1json/schema")
                    .map(|s| self_contained(s, &schemas))
                    .unwrap_or_else(|| serde_json::json!({"type": "object", "properties": {}, "additionalProperties": false}));
                tools.push((
                    t["name"].as_str().expect("x-mcp-tool.name").to_string(),
                    t["description"].as_str().expect("x-mcp-tool.description").trim().to_string(),
                    method.to_uppercase(),
                    p.clone(),
                    serde_json::to_string(&input).expect("json"),
                ));
            }
        }
    }
    o.push_str("#[derive(Debug, Clone, Copy, PartialEq, Eq)]\npub enum Scope { Public, Ingest, Query, Admin }\n\n");
    o.push_str("pub struct Operation { pub method: &'static str, pub path: &'static str, pub id: &'static str, pub scope: Scope }\n\n");
    writeln!(o, "pub const OPERATIONS: &[Operation] = &[").unwrap();
    for (m, p, id, sc) in &ops {
        writeln!(o, "    Operation {{ method: {m:?}, path: {p:?}, id: {id:?}, scope: Scope::{} }},", camel(sc)).unwrap();
    }
    o.push_str("];\n\n");
    o.push_str("/// Every route of the spec, each wired to the handler named by its operationId.\npub fn router(state: crate::AppState) -> axum::Router {\n    axum::Router::new()\n");
    let mut by_path: BTreeMap<&str, Vec<(&str, &str)>> = BTreeMap::new();
    for (m, p, id, _) in &ops {
        by_path.entry(p).or_default().push((m, id));
    }
    for (p, ms) in by_path {
        let chain = ms.iter().map(|(m, id)| format!("{}(crate::handlers::{id})", m.to_lowercase())).collect::<Vec<_>>().join(".");
        writeln!(o, "        .route({p:?}, axum::routing::{chain})").unwrap();
    }
    o.push_str("        .with_state(state)\n}\n\n");
    o.push_str("pub struct McpTool { pub name: &'static str, pub description: &'static str, pub method: &'static str, pub path: &'static str, pub input_schema: &'static str }\n\n");
    writeln!(o, "pub const MCP_TOOLS: &[McpTool] = &[").unwrap();
    for (n, d, m, p, s) in &tools {
        writeln!(o, "    McpTool {{ name: {n:?}, description: {d:?}, method: {m:?}, path: {p:?}, input_schema: {s:?} }},").unwrap();
    }
    o.push_str("];\n\n");
    writeln!(o, "pub const OPENAPI_YAML: &str = include_str!({path:?});").unwrap();
    let out = std::path::Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("api.rs");
    std::fs::write(out, o).expect("write api.rs");
}
