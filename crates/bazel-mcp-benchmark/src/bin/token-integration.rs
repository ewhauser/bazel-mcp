use std::{collections::BTreeSet, path::PathBuf};

use anyhow::{Context, ensure};
use bazel_mcp_benchmark::{HarnessConfig, ProjectManifest, run_integration};
use clap::Parser;

#[derive(Debug, Parser)]
struct Args {
    #[arg(long, default_value = "abseil-cpp")]
    project: String,
    #[arg(long, default_value = "o200k_base")]
    encoding: String,
    #[arg(long, default_value_t = 5)]
    samples: u32,
    #[arg(long = "scenario")]
    scenarios: Vec<String>,
    #[arg(long, default_value = "both")]
    cache_condition: String,
    #[arg(long)]
    assert_gates: bool,
    #[arg(long)]
    keep_worktree: bool,
    #[arg(long)]
    live_agent: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    if args.live_agent {
        anyhow::bail!("live-agent mode requires a provider adapter and is not configured")
    }
    ensure!(args.samples > 0, "--samples must be greater than zero");
    let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repository_root = crate_root
        .parent()
        .and_then(|path| path.parent())
        .context("benchmark crate must be inside the workspace crates directory")?
        .to_owned();
    let path = crate_root
        .join("resources/projects")
        .join(format!("{}.toml", args.project));
    let manifest = ProjectManifest::load(&path)?;
    let cache_conditions = match args.cache_condition.as_str() {
        "cold" => vec!["cold".to_owned()],
        "warm" => vec!["warm".to_owned()],
        "both" => vec!["cold".to_owned(), "warm".to_owned()],
        value => anyhow::bail!("unsupported cache condition {value:?}; use cold, warm, or both"),
    };
    let report = run_integration(HarnessConfig {
        repository_root,
        project: manifest,
        encoding: args.encoding,
        samples: args.samples,
        assert_gates: args.assert_gates,
        scenario_filter: args.scenarios.into_iter().collect::<BTreeSet<_>>(),
        cache_conditions,
        keep_worktree: args.keep_worktree,
    })
    .await?;
    print!("{}", report.markdown());
    Ok(())
}
