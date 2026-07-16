use std::{collections::BTreeSet, path::PathBuf, time::Duration};

use anyhow::{Context, ensure};
use bazel_mcp_benchmark::{
    AgenticAdapter, AgenticConfig, AgenticProjectManifest, run_agentic_benchmark,
};
use clap::Parser;

#[derive(Debug, Parser)]
#[command(about = "Run paired Codex coding tasks against shell and bazel-mcp adapters")]
struct Args {
    #[arg(long, default_value = "abseil-cpp")]
    project: String,
    #[arg(long, default_value_t = 1)]
    samples: u32,
    #[arg(long = "task")]
    tasks: Vec<String>,
    #[arg(long = "adapter", value_enum)]
    adapters: Vec<AgenticAdapter>,
    #[arg(long)]
    keep_worktrees: bool,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    reasoning_effort: Option<String>,
    #[arg(long, value_name = "PATH")]
    codex_executable: Option<PathBuf>,
    #[arg(long, value_name = "PATH")]
    proxy_executable: Option<PathBuf>,
    #[arg(long)]
    timeout_seconds: Option<u64>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    ensure!(args.samples > 0, "--samples must be greater than zero");
    if let Some(timeout) = args.timeout_seconds {
        ensure!(timeout > 0, "--timeout-seconds must be greater than zero");
    }
    let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repository_root = crate_root
        .parent()
        .and_then(|path| path.parent())
        .context("benchmark crate must be inside the workspace crates directory")?
        .to_owned();
    let manifest = AgenticProjectManifest::load(
        &crate_root
            .join("resources/agentic")
            .join(format!("{}.toml", args.project)),
    )?;
    let codex_executable = args
        .codex_executable
        .or_else(|| which_on_path("codex"))
        .context("Codex CLI is required for the agentic benchmark")?;
    let proxy_executable = args
        .proxy_executable
        .unwrap_or_else(|| repository_root.join("target/debug/bazel-agentic-shell"));
    let adapters = if args.adapters.is_empty() {
        vec![AgenticAdapter::ShellDefault, AgenticAdapter::BazelMcpToon]
    } else {
        args.adapters
    };
    let report = run_agentic_benchmark(AgenticConfig {
        repository_root,
        project: manifest,
        samples: args.samples,
        task_filter: args.tasks.into_iter().collect::<BTreeSet<_>>(),
        adapters,
        keep_worktrees: args.keep_worktrees,
        codex_executable,
        proxy_executable,
        model: args.model,
        reasoning_effort: args.reasoning_effort,
        timeout_override: args.timeout_seconds.map(Duration::from_secs),
    })
    .await?;
    print!("{}", report.markdown());
    Ok(())
}

fn which_on_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}
