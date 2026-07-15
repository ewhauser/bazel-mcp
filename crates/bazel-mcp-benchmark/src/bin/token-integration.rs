use std::{collections::BTreeSet, path::PathBuf, time::Duration};

use anyhow::{Context, ensure};
use bazel_mcp_benchmark::{
    CodexLiveConfig, HarnessConfig, ProjectManifest, assert_acceptance_gates, recompute_report,
    run_codex_live_agent, run_integration,
};
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
    #[arg(long)]
    live_agent_model: Option<String>,
    #[arg(long, value_name = "PATH")]
    codex_executable: Option<PathBuf>,
    #[arg(long, default_value_t = 7_200)]
    live_agent_timeout_seconds: u64,
    /// Recompute schema-v3 statistics for an existing completed report.
    #[arg(long, value_name = "REPORT_JSON")]
    recompute_report: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    ensure!(args.samples > 0, "--samples must be greater than zero");
    ensure!(
        args.live_agent_timeout_seconds > 0,
        "--live-agent-timeout-seconds must be greater than zero"
    );
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
    if let Some(path) = &args.recompute_report {
        let report = recompute_report(path).await?;
        if args.assert_gates {
            ensure!(
                report
                    .samples
                    .iter()
                    .map(|sample| sample.sample)
                    .collect::<BTreeSet<_>>()
                    .len()
                    >= 5,
                "an acceptance report requires at least five measured samples"
            );
            assert_acceptance_gates(&report, &manifest.scenarios)?;
        }
        print!("{}", report.markdown());
        return Ok(());
    }
    let cache_conditions = match args.cache_condition.as_str() {
        "cold" => vec!["cold".to_owned()],
        "warm" => vec!["warm".to_owned()],
        "both" => vec!["cold".to_owned(), "warm".to_owned()],
        value => anyhow::bail!("unsupported cache condition {value:?}; use cold, warm, or both"),
    };
    if args.live_agent {
        let codex_executable = args
            .codex_executable
            .or_else(|| which_on_path("codex"))
            .context("Codex CLI is required for --live-agent")?;
        let report = run_codex_live_agent(CodexLiveConfig {
            repository_root,
            project: manifest,
            samples: args.samples,
            scenario_filter: args.scenarios.into_iter().collect(),
            cache_conditions,
            keep_worktree: args.keep_worktree,
            codex_executable,
            model: args.live_agent_model,
            timeout: Duration::from_secs(args.live_agent_timeout_seconds),
        })
        .await?;
        print!("{}", report.markdown());
        return Ok(());
    }
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

fn which_on_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}
