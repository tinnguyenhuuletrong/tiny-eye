//! `tiny-eye` — one binary (SPEC §3). A node runs ingest, coordinator and query together;
//! several nodes sharing `storage.url` form a fleet and answer over each other's data
//! (docs/examples/run-a-fleet.md). `mcp` runs the stdio MCP bridge against a running node.
//!
//! The subcommands and flags are listed once, in [`USAGE`], which `tiny-eye --help` prints.

use anyhow::{Context, Result, bail};
use engine::{Config, Node};
use std::path::PathBuf;

/// The command line, printed by `tiny-eye --help`. The parser below accepts exactly these flags.
const USAGE: &str = "\
tiny-eye: metrics, logs and traces in one binary

Usage:
  tiny-eye [--config <file.yaml>]       run a node: ingest, query and the web UI
  tiny-eye mcp [--url <node url>] [--token-env <VAR>]
                                        stdio MCP server for AI agents, forwarding to a running
                                        node (default url http://127.0.0.1:7070; the bearer
                                        token is read from the environment variable VAR)
  tiny-eye config [--config <file.yaml>]
                                        print the effective configuration (secrets redacted)
  tiny-eye schemas [--root <dir>]       write the editor and agent grammars under the repo root
                                        (schema/generated, docs/reference; default root .)
  tiny-eye -h | --help                  print this help
  tiny-eye -V | --version               print the version

Without --config, the built-in defaults apply (config/default.yaml in the repository).
TINY_EYE__<SECTION>__<KEY> environment variables override single keys; RUST_LOG sets the log
level (default info). Docs: https://tinnguyenhuuletrong.github.io/tiny-eye/
";

#[derive(Debug, PartialEq)]
enum Cmd {
    Help,
    Version,
    Run { config: Option<PathBuf> },
    Mcp { url: String, token_env: Option<String> },
    Config { config: Option<PathBuf> },
    Schemas { root: String },
}

/// Parses the arguments after the program name. Every flag takes a value; anything not listed
/// in [`USAGE`] is an error, so a typo never starts a server with the defaults.
fn parse(args: &[String]) -> Result<Cmd> {
    let (cmd, rest) = match args.first().map(String::as_str) {
        Some(c) if !c.starts_with('-') => (c, &args[1..]),
        _ => ("", args),
    };
    let flags: &[&str] = match cmd {
        "" => &["--config", "--role"],
        "mcp" => &["--url", "--token-env"],
        "config" => &["--config"],
        "schemas" => &["--root"],
        "help" => return Ok(Cmd::Help),
        c => bail!("unknown command {c:?}; run tiny-eye --help"),
    };
    let mut values = std::collections::HashMap::new();
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => return Ok(Cmd::Help),
            "-V" | "--version" => return Ok(Cmd::Version),
            f if flags.contains(&f) => {
                let v = it.next().filter(|v| !v.starts_with('-')).with_context(|| format!("{f} needs a value; run tiny-eye --help"))?;
                values.insert(f, v.clone());
            }
            f if f.starts_with('-') => bail!("unknown option {f}; run tiny-eye --help"),
            f => bail!("unexpected argument {f:?}; run tiny-eye --help"),
        }
    }
    let config = values.remove("--config").map(PathBuf::from);
    Ok(match cmd {
        "mcp" => Cmd::Mcp {
            url: values.remove("--url").unwrap_or_else(|| "http://127.0.0.1:7070".into()),
            token_env: values.remove("--token-env"),
        },
        "config" => Cmd::Config { config },
        "schemas" => Cmd::Schemas { root: values.remove("--root").unwrap_or_else(|| ".".into()) },
        _ => {
            if let Some(role) = values.remove("--role")
                && role != "all"
            {
                bail!(
                    "--role {role}: only --role all exists; to run several nodes, share storage.url as a fleet (docs/examples/run-a-fleet.md)"
                );
            }
            Cmd::Run { config }
        }
    })
}

/// What to do next, printed once the node is listening. `exe` is this binary, so the MCP line
/// can be pasted from any directory.
fn banner(listen: &str, exe: &str) -> String {
    let url = format!("http://{}", listen.replace("0.0.0.0", "127.0.0.1").replace("[::]", "127.0.0.1"));
    format!(
        "\n  tiny-eye {} is ready\n\n  \
         Web UI        {url}\n  \
         OTLP/HTTP     OTEL_EXPORTER_OTLP_ENDPOINT={url}\n  \
         Remote-write  {url}/api/v1/write\n  \
         Query (TQS)   curl {url}/api/v1/query -d @api/examples/error-logs.json\n  \
         AI agents     claude mcp add tiny-eye -- {exe} mcp --url {url}\n",
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
    let config_path = match parse(&args)? {
        Cmd::Help => {
            print!("{USAGE}");
            return Ok(());
        }
        Cmd::Version => {
            println!("tiny-eye {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Cmd::Mcp { url, token_env } => {
            let token = token_env.map(|v| std::env::var(&v).with_context(|| format!("{v} is not set"))).transpose()?;
            return tokio::task::spawn_blocking(move || server::mcp::serve(&url, token)).await?;
        }
        Cmd::Schemas { root } => {
            for f in server::schemas::write(std::path::Path::new(&root))? {
                println!("{f}");
            }
            return Ok(());
        }
        Cmd::Config { config } => {
            println!("{}", Config::load(config.as_deref())?.redacted_yaml());
            return Ok(());
        }
        Cmd::Run { config } => config,
    };
    let cfg = Config::load(config_path.as_deref())?;
    tracing::debug!("effective config (tiny-eye config prints it):\n{}", cfg.redacted_yaml());
    let dashboards = engine::dashboards::load(&cfg.dashboards.dir)?;
    let listen = cfg.server.listen.clone();
    let node = Node::open(cfg).await?;
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let bg = tokio::spawn(node.clone().run_background(stop_rx));
    let state = server::AppState::new(node.clone(), dashboards)?;
    let listener = tokio::net::TcpListener::bind(&listen).await.with_context(|| format!("binding {listen}"))?;
    tracing::info!(%listen, "serving");
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "tiny-eye".into());
    eprintln!("{}", banner(&listen, &exe));
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
    let ingested: u64 = node.metrics.ingested_rows.iter().map(|r| r.load(std::sync::atomic::Ordering::Relaxed)).sum();
    tracing::info!("stopped: every buffered row is in Parquet and the catalog ({ingested} rows ingested since start)");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_line(line: &str) -> Result<Cmd> {
        parse(&line.split_whitespace().map(String::from).collect::<Vec<_>>())
    }

    fn p(line: &str) -> Cmd {
        parse_line(line).expect(line)
    }

    fn err(line: &str) -> String {
        format!("{:#}", parse_line(line).expect_err(line))
    }

    #[test]
    fn help_and_version() {
        for l in ["-h", "--help", "help", "mcp --help", "--config x.yaml -h"] {
            assert_eq!(p(l), Cmd::Help, "{l}");
        }
        for l in ["-V", "--version", "config -V"] {
            assert_eq!(p(l), Cmd::Version, "{l}");
        }
    }

    #[test]
    fn commands_and_their_flags() {
        assert_eq!(p(""), Cmd::Run { config: None });
        assert_eq!(p("--config a.yaml --role all"), Cmd::Run { config: Some("a.yaml".into()) });
        assert_eq!(p("mcp"), Cmd::Mcp { url: "http://127.0.0.1:7070".into(), token_env: None });
        assert_eq!(p("mcp --url http://h:1 --token-env T"), Cmd::Mcp { url: "http://h:1".into(), token_env: Some("T".into()) });
        assert_eq!(p("config --config a.yaml"), Cmd::Config { config: Some("a.yaml".into()) });
        assert_eq!(p("schemas"), Cmd::Schemas { root: ".".into() });
    }

    #[test]
    fn mistakes_point_at_help_instead_of_starting_a_server() {
        assert_eq!(err("--confg dev.yaml"), "unknown option --confg; run tiny-eye --help");
        assert_eq!(err("mcp --config a.yaml"), "unknown option --config; run tiny-eye --help", "flags are per command");
        assert_eq!(err("serve"), "unknown command \"serve\"; run tiny-eye --help");
        assert_eq!(err("--config"), "--config needs a value; run tiny-eye --help");
        assert_eq!(err("config extra"), "unexpected argument \"extra\"; run tiny-eye --help");
        assert!(err("--role query").starts_with("--role query: only --role all exists"));
    }

    #[test]
    fn usage_lists_every_flag_the_parser_accepts() {
        for f in ["--config", "--url", "--token-env", "--root", "mcp", "config", "schemas", "--help", "--version"] {
            assert!(USAGE.contains(f), "{f} missing from USAGE");
        }
    }
}
