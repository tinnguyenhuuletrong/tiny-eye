//! MCP server over stdio (SPEC §10): `tiny-eye mcp --url http://host:7070 [--token-env VAR]`.
//!
//! A thin JSON-RPC bridge: the tool list is generated from api/openapi.yaml (`x-mcp-tool`),
//! each call is forwarded to the operation's HTTP endpoint, so the agent gets exactly the API's
//! validation (every problem code with its JSON path) and its read-only scope. Telemetry text is
//! returned inside a delimited data block, never as tool prose (limits prompt injection).

use crate::api::MCP_TOOLS;
use anyhow::Result;
use serde_json::{Value, json};
use std::io::{BufRead, Write};

pub const PROTOCOL_VERSION: &str = "2025-06-18";

pub fn serve(base_url: &str, token: Option<String>) -> Result<()> {
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            writeln!(out, "{}", json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "parse error"}}))?;
            continue;
        };
        let Some(id) = msg.get("id").cloned() else { continue }; // notification
        let result = handle(&agent, base_url, token.as_deref(), &msg);
        let resp = match result {
            Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
            Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32603, "message": format!("{e:#}")}}),
        };
        writeln!(out, "{resp}")?;
        out.flush()?;
    }
    Ok(())
}

pub fn tools_list() -> Value {
    json!({"tools": MCP_TOOLS.iter().map(|t| json!({
        "name": t.name,
        "description": t.description,
        "inputSchema": serde_json::from_str::<Value>(t.input_schema).unwrap_or(json!({"type": "object"})),
    })).collect::<Vec<_>>()})
}

fn handle(agent: &ureq::Agent, base: &str, token: Option<&str>, msg: &Value) -> Result<Value> {
    match msg["method"].as_str().unwrap_or_default() {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "tiny-eye", "version": env!("CARGO_PKG_VERSION")},
            "instructions": "Query metrics, traces and logs with TQS 3. Start with discover_telemetry_catalog, then validate_tqs / run_tqs."
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(tools_list()),
        "tools/call" => {
            let name = msg["params"]["name"].as_str().unwrap_or_default();
            let tool = MCP_TOOLS.iter().find(|t| t.name == name).ok_or_else(|| anyhow::anyhow!("unknown tool {name:?}"))?;
            let args = msg["params"].get("arguments").cloned().unwrap_or(json!({}));
            let url = format!("{}{}", base.trim_end_matches('/'), tool.path);
            let resp = match (tool.method, token) {
                ("GET", Some(t)) => agent.get(&url).header("Authorization", &format!("Bearer {t}")).call(),
                ("GET", None) => agent.get(&url).call(),
                (_, Some(t)) => agent
                    .post(&url)
                    .header("Authorization", &format!("Bearer {t}"))
                    .header("Content-Type", "application/json")
                    .send(args.to_string()),
                (_, None) => agent.post(&url).header("Content-Type", "application/json").send(args.to_string()),
            };
            let mut resp = resp?;
            let status = resp.status().as_u16();
            let body = resp.body_mut().read_to_string()?;
            Ok(json!({
                "content": [{"type": "text", "text": format!("<data status=\"{status}\">\n{body}\n</data>")}],
                "isError": status >= 400,
            }))
        }
        m => anyhow::bail!("method {m:?} not supported"),
    }
}
