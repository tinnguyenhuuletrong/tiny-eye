//! `tiny-eye` — one binary (SPEC §3). Single-node mode (`--role all`) runs ingest, coordinator
//! and query together; `mcp` runs the stdio MCP bridge against a running node.
//!
//!   tiny-eye [--config <file.yaml>]
//!   tiny-eye mcp --url http://127.0.0.1:7070 [--token-env VAR]
//!   tiny-eye config            print the effective configuration (secrets redacted)

use anyhow::{Context, Result, bail};
use engine::{Config, Node};
use std::path::PathBuf;

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let config_path = arg(&args, "--config").map(PathBuf::from);
    match args.first().map(String::as_str) {
        Some("mcp") => {
            let url = arg(&args, "--url").unwrap_or_else(|| "http://127.0.0.1:7070".into());
            let token = arg(&args, "--token-env").map(|v| std::env::var(&v).with_context(|| format!("{v} is not set"))).transpose()?;
            return tokio::task::spawn_blocking(move || server::mcp::serve(&url, token)).await?;
        }
        Some("config") => {
            println!("{}", Config::load(config_path.as_deref())?.redacted_yaml());
            return Ok(());
        }
        Some(a) if !a.starts_with("--") => bail!("unknown command {a:?}; see the header of crates/server/src/main.rs"),
        _ => {}
    }
    if let Some(role) = arg(&args, "--role")
        && role != "all"
    {
        bail!("--role {role}: only single-node mode (--role all) is built; cluster roles are milestone M3");
    }
    let cfg = Config::load(config_path.as_deref())?;
    tracing::info!("effective config:\n{}", cfg.redacted_yaml());
    let dashboards = engine::dashboards::load(&cfg.dashboards.dir)?;
    let listen = cfg.server.listen.clone();
    let node = Node::open(cfg).await?;
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let bg = tokio::spawn(node.clone().run_background(stop_rx));
    let state = server::AppState::new(node.clone(), dashboards)?;
    let listener = tokio::net::TcpListener::bind(&listen).await.with_context(|| format!("binding {listen}"))?;
    tracing::info!(%listen, "serving");
    axum::serve(listener, server::app(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutting down: flushing buffered rows");
        })
        .await?;
    let _ = stop_tx.send(true);
    let _ = bg.await;
    node.flush_and_checkpoint().await?;
    Ok(())
}
