//! TQS conformance suite (SPEC §15.1): every metric case is compared point by point with
//! Prometheus on the same samples, spans/logs cases with an independent Rust computation,
//! validation cases with their expected problem code, and 16 mutants must each turn at
//! least one case red. Seeded from the research suite; run with `make conformance`.

#![allow(clippy::unwrap_used, clippy::type_complexity, clippy::enum_variant_names, dead_code)]

mod fixtures;
mod promref;
mod suite;
mod synth;
mod write;

/// Lets the seeded suite keep its `crate::tqs::…` paths.
use ::tqs;

fn main() -> anyhow::Result<()> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let tools = std::env::var("TOOLS_DIR").map(std::path::PathBuf::from).unwrap_or_else(|_| root.join(".tools"));
    if !tools.join("prometheus").exists() {
        anyhow::bail!("{} missing: run `make tools` first", tools.join("prometheus").display());
    }
    let out = suite::run_all(&tools, &root.join(".state/tqs"))?;
    let failed = out.iter().filter(|c| !c.pass).count();
    println!("\n{}/{} cases pass", out.len() - failed, out.len());
    if failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}
