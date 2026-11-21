//! Research code for a blob-first observability engine on S3 (MinIO) + DuckLake.
//! It proves or refutes design claims; it is not the product.
//!
//! `blobobs-poc lake` — storage experiments E1–E9 (needs `scripts/stack.sh up`).
//! `blobobs-poc tqs`  — query-schema coverage suite against Prometheus (no stack needed).

mod exp;
mod fixtures;
mod lake;
mod promref;
mod suite;
mod synth;
mod tqs;
mod write;

use anyhow::{Result, bail};
use std::fmt::Write as _;

#[tokio::main]
async fn main() -> Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("lake") => lake_experiments().await,
        Some("tqs") => {
            let out = suite::run_all(std::path::Path::new(".tools"), std::path::Path::new(".state/tqs"))?;
            if out.iter().any(|c| !c.pass) {
                std::process::exit(1);
            }
            Ok(())
        }
        Some("tqs-lake") => {
            let out = suite::run_on_lake(&lake::Env::from_process()?, std::path::Path::new(".state/tqs-lake"))?;
            let mut md = String::from("# TQS on DuckLake + MinIO\n\nThe same compiled queries, run on DuckLake tables stored in MinIO, against the in-memory result.\n\n| ID | Function | Result | Detail |\n| :--- | :--- | :--- | :--- |\n");
            for c in &out {
                println!("[{}] {:<10} {:<40} {}", if c.pass { "PASS" } else { "FAIL" }, c.id, c.function, c.detail);
                md.push_str(&format!("| {} | {} | {} | {} |\n", c.id, c.function, if c.pass { "PASS" } else { "**FAIL**" }, c.detail));
            }
            std::fs::write("results/tqs-lake.md", md)?;
            if out.iter().any(|c| !c.pass) {
                std::process::exit(1);
            }
            Ok(())
        }
        other => bail!("usage: blobobs-poc lake|tqs|tqs-lake (got {other:?})"),
    }
}

async fn lake_experiments() -> Result<()> {
    let env = lake::Env::from_process()?;
    let version: String = env.duckdb()?.query_row("SELECT version()", [], |r| r.get(0))?;
    let findings = exp::run_all(env).await?;
    let passed = findings.iter().filter(|f| f.pass).count();
    let mut md = String::new();
    writeln!(md, "# PoC results\n\nDuckDB {version} + DuckLake (PostgreSQL 16 catalog) + MinIO; Rust writer (parquet 60, object_store 0.14).\n")?;
    writeln!(md, "**{passed}/{} experiments passed.**\n", findings.len())?;
    for f in &findings {
        writeln!(md, "## {} — {} — {}\n", f.id, if f.pass { "PASS" } else { "FAIL" }, f.claim)?;
        for e in &f.evidence {
            writeln!(md, "- {e}")?;
        }
        writeln!(md)?;
    }
    std::fs::write("results/poc-results.md", &md)?;
    println!("\n{passed}/{} passed; written to results/poc-results.md", findings.len());
    if passed != findings.len() {
        std::process::exit(1);
    }
    Ok(())
}
