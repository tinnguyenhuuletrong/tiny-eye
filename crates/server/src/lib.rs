//! tiny-eye HTTP server. Routes, auth scopes, response types and MCP tools are generated from
//! `api/openapi.yaml` (see build.rs); this crate holds the handlers and the protocol decoders.

pub mod handlers;
pub mod mcp;
pub mod receivers;

#[allow(clippy::all)]
pub mod api {
    include!(concat!(env!("OUT_DIR"), "/api.rs"));
}

use api::Scope;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use engine::Node;
use engine::config::Config;
use engine::dashboards::Dashboard;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub node: Arc<Node>,
    pub dashboards: Arc<Vec<Dashboard>>,
    /// token → scopes; empty = auth disabled.
    pub tokens: Arc<HashMap<String, Vec<Scope>>>,
}

impl AppState {
    pub fn new(node: Arc<Node>, dashboards: Vec<Dashboard>) -> anyhow::Result<AppState> {
        let tokens = load_tokens(&node.cfg)?;
        Ok(AppState { node, dashboards: Arc::new(dashboards), tokens: Arc::new(tokens) })
    }
}

fn load_tokens(cfg: &Config) -> anyhow::Result<HashMap<String, Vec<Scope>>> {
    let mut out = HashMap::new();
    for t in &cfg.auth.tokens {
        let token = std::env::var(&t.token_env).map_err(|_| anyhow::anyhow!("auth token env {} is not set", t.token_env))?;
        let scopes = t
            .scopes
            .iter()
            .map(|s| match s {
                engine::config::Scope::Ingest => Scope::Ingest,
                engine::config::Scope::Query => Scope::Query,
                engine::config::Scope::Admin => Scope::Admin,
            })
            .collect();
        out.insert(token, scopes);
    }
    Ok(out)
}

/// The scope an incoming request needs, looked up in the generated operation table.
fn required_scope(method: &str, path: &str) -> Scope {
    let segs: Vec<&str> = path.trim_end_matches('/').split('/').collect();
    api::OPERATIONS
        .iter()
        .find(|op| {
            let pat: Vec<&str> = op.path.split('/').collect();
            op.method == method && pat.len() == segs.len() && pat.iter().zip(&segs).all(|(p, s)| p.starts_with('{') || p == s)
        })
        .map(|op| op.scope)
        .unwrap_or(Scope::Public)
}

/// Bearer-token auth (SPEC §11): each operation's `x-scope` must be granted; `admin` grants all.
pub async fn auth(State(st): State<AppState>, req: Request, next: Next) -> Response {
    if st.tokens.is_empty() {
        return next.run(req).await;
    }
    let need = required_scope(req.method().as_str(), req.uri().path());
    if need == Scope::Public {
        return next.run(req).await;
    }
    let granted = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .and_then(|t| st.tokens.get(t))
        .is_some_and(|scopes| scopes.contains(&need) || scopes.contains(&Scope::Admin));
    if granted { next.run(req).await } else { (StatusCode::UNAUTHORIZED, "missing or insufficient bearer token\n").into_response() }
}

/// The full HTTP app: generated routes + auth + body limit.
pub fn app(state: AppState) -> axum::Router {
    api::router(state.clone())
        .layer(axum::middleware::from_fn_with_state(state, auth))
        .layer(axum::extract::DefaultBodyLimit::max(64 << 20))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_come_from_the_spec() {
        assert_eq!(required_scope("POST", "/api/v1/write"), Scope::Ingest);
        assert_eq!(required_scope("POST", "/api/v1/query"), Scope::Query);
        assert_eq!(required_scope("GET", "/api/v1/dashboards/abc"), Scope::Query);
        assert_eq!(required_scope("GET", "/healthz"), Scope::Public);
    }

    #[test]
    fn every_mcp_tool_schema_is_json() {
        assert!(!api::MCP_TOOLS.is_empty());
        for t in api::MCP_TOOLS {
            let v: serde_json::Value = serde_json::from_str(t.input_schema).expect("tool schema is JSON");
            assert_eq!(v["type"], "object", "{}", t.name);
        }
    }
}
