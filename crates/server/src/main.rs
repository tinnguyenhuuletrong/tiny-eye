//! `tiny-eye` — one binary (SPEC §3). Single-node mode (`--role all`) runs ingest, coordinator
//! and query together; `mcp` runs the stdio MCP bridge against a running node.
//!
//!   tiny-eye [--config <file.yaml>]
//!   tiny-eye mcp --url http://127.0.0.1:7070 [--token-env VAR]
//!   tiny-eye config            print the effective configuration (secrets redacted)
//!   tiny-eye schemas [--root .]   write editor/agent grammars under the repo root (schema/generated, docs/reference)

use anyhow::{Context, Result, bail};
use engine::{Config, Node};
use std::path::PathBuf;

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

/// What to do next, printed once the node is listening.
fn banner(listen: &str) -> String {
    let url = format!("http://{}", listen.replace("0.0.0.0", "127.0.0.1").replace("[::]", "127.0.0.1"));
    format!(
        "\n  tiny-eye {} is ready\n\n  \
         Web UI        {url}\n  \
         OTLP/HTTP     OTEL_EXPORTER_OTLP_ENDPOINT={url}\n  \
         Remote-write  {url}/api/v1/write\n  \
         Query (TQS)   curl {url}/api/v1/query -d @query.json\n  \
         AI agents     claude mcp add tiny-eye -- tiny-eye mcp --url {url}\n",
        env!("CARGO_PKG_VERSION")
    )
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
        Some("schemas") => {
            let root = arg(&args, "--root").unwrap_or_else(|| ".".into());
            for f in server::schemas::write(std::path::Path::new(&root))? {
                println!("{f}");
            }
            return Ok(());
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
    eprintln!("{}", banner(&listen));
    axum::serve(listener, server::app(state))
        .with_graceful_shutdown(async {
            // SIGINT (Ctrl-C) or SIGTERM (containers, systemd): stop serving, then flush.
            let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} }
            tracing::info!("shutting down: flushing buffered rows");
        })
        .await?;
    let _ = stop_tx.send(true);
    let _ = bg.await;
    node.flush_and_checkpoint().await?;
    Ok(())
}
