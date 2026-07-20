use std::{
    collections::BTreeSet,
    fs::OpenOptions,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::{EnvironmentMetadata, ProjectManifest, Scenario};

const MAX_CODEX_EVENT_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LiveAdapter {
    ShellDefault,
    ShellOptimized,
    BazelMcp,
}

impl LiveAdapter {
    const ALL: [Self; 3] = [Self::ShellDefault, Self::ShellOptimized, Self::BazelMcp];

    const fn name(self) -> &'static str {
        match self {
            Self::ShellDefault => "shell-default",
            Self::ShellOptimized => "shell-optimized",
            Self::BazelMcp => "bazel-mcp",
        }
    }
}

#[derive(Clone, Debug)]
pub struct CodexLiveConfig {
    pub repository_root: PathBuf,
    pub project: ProjectManifest,
    pub samples: u32,
    pub scenario_filter: BTreeSet<String>,
    pub cache_conditions: Vec<String>,
    pub keep_worktree: bool,
    pub codex_executable: PathBuf,
    pub model: Option<String>,
    pub timeout: Duration,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ProviderUsage {
    pub(crate) input_tokens: u64,
    pub(crate) cached_input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) reasoning_output_tokens: u64,
}

impl ProviderUsage {
    pub(crate) fn uncached_input_tokens(&self) -> u64 {
        self.input_tokens.saturating_sub(self.cached_input_tokens)
    }

    pub(crate) fn total_tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LiveAgentSample {
    adapter: String,
    scenario: String,
    cache_condition: String,
    sample: u32,
    expected_exit: i32,
    reported_exit: i32,
    expected_cause: Option<String>,
    diagnostic_found: bool,
    used_expected_tool: bool,
    end_to_end_ms: u64,
    model_events: u64,
    tool_calls: u64,
    usage: ProviderUsage,
    final_summary: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LiveAgentSummary {
    adapter: String,
    observations: usize,
    correct_observations: usize,
    input_tokens: u64,
    cached_input_tokens: u64,
    uncached_input_tokens: u64,
    output_tokens: u64,
    reasoning_output_tokens: u64,
    total_tokens: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LiveAgentComparison {
    baseline_adapter: String,
    candidate_adapter: String,
    input_token_reduction_percent: f64,
    uncached_input_token_reduction_percent: f64,
    total_token_reduction_percent: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LiveAgentReport {
    schema_version: u32,
    provider: String,
    provider_version: String,
    model: Option<String>,
    project: String,
    commit: String,
    bazel_version: String,
    environment: EnvironmentMetadata,
    samples: Vec<LiveAgentSample>,
    summaries: Vec<LiveAgentSummary>,
    comparisons: Vec<LiveAgentComparison>,
}

impl LiveAgentReport {
    pub fn markdown(&self) -> String {
        let mut output = format!(
            "# Bazel MCP live-agent token integration\n\nProvider: `{}` (`{}`)  \nModel: `{}`  \nProject: `{}` @ `{}`  \nBazel: `{}`\n\n",
            self.provider,
            self.provider_version,
            self.model.as_deref().unwrap_or("provider default"),
            self.project,
            self.commit,
            self.bazel_version,
        );
        output.push_str(
            "| Adapter | N | Correct | Input | Cached input | Uncached input | Output | Reasoning | Total |\n",
        );
        output.push_str("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n");
        for summary in &self.summaries {
            output.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
                summary.adapter,
                summary.observations,
                summary.correct_observations,
                summary.input_tokens,
                summary.cached_input_tokens,
                summary.uncached_input_tokens,
                summary.output_tokens,
                summary.reasoning_output_tokens,
                summary.total_tokens,
            ));
        }
        output.push_str("\n| Baseline | Input-token reduction | Uncached-input reduction | Total-token reduction |\n");
        output.push_str("| --- | ---: | ---: | ---: |\n");
        for comparison in &self.comparisons {
            output.push_str(&format!(
                "| {} | {:.2}% | {:.2}% | {:.2}% |\n",
                comparison.baseline_adapter,
                comparison.input_token_reduction_percent,
                comparison.uncached_input_token_reduction_percent,
                comparison.total_token_reduction_percent,
            ));
        }
        output
    }
}

#[derive(Debug, Deserialize)]
struct AgentVerdict {
    exit_code: i32,
    root_cause: String,
    summary: String,
}

#[derive(Debug, Serialize)]
struct ServerConfig {
    allowed_roots: Vec<PathBuf>,
    cache_root: PathBuf,
    bazel_executable: PathBuf,
    output_user_root: PathBuf,
    environment_allowlist: BTreeSet<String>,
}

#[derive(Debug)]
struct CodexObservation {
    verdict: AgentVerdict,
    usage: ProviderUsage,
    end_to_end_ms: u64,
    model_events: u64,
    tool_calls: u64,
    used_expected_tool: bool,
}

pub async fn run_codex_live_agent(config: CodexLiveConfig) -> anyhow::Result<LiveAgentReport> {
    ensure!(
        config.samples > 0,
        "live-agent samples must be greater than zero"
    );
    ensure!(
        config.codex_executable.is_file(),
        "Codex executable is missing"
    );
    let run_id = SystemTime::UNIX_EPOCH.elapsed()?.as_millis().to_string();
    let run_root = config
        .repository_root
        .join(".cache/benchmarks")
        .join(&config.project.name)
        .join(format!("live-{run_id}"));
    let worktree = run_root.join("worktree");
    let corpus = config
        .repository_root
        .join(".cache/corpora")
        .join(&config.project.name)
        .join(&config.project.commit);
    ensure!(
        corpus.join(".git").exists(),
        "corpus is missing; run make setup-oss-corpus"
    );
    tokio::fs::create_dir_all(&run_root).await?;
    create_worktree(&corpus, &worktree, &config.project.commit).await?;
    install_overlay(&config.repository_root, &worktree).await?;
    let schema_path = run_root.join("agent-verdict.schema.json");
    tokio::fs::write(&schema_path, verdict_schema()).await?;

    let scenarios: Vec<_> = config
        .project
        .scenarios
        .iter()
        .filter(|scenario| {
            config.scenario_filter.is_empty() || config.scenario_filter.contains(&scenario.name)
        })
        .cloned()
        .collect();
    ensure!(!scenarios.is_empty(), "no live-agent scenarios selected");

    let mut samples = Vec::new();
    for cache in &config.cache_conditions {
        ensure!(
            matches!(cache.as_str(), "cold" | "warm"),
            "cache condition must be cold or warm"
        );
        for sample in 0..config.samples {
            for scenario in &scenarios {
                for adapter in LiveAdapter::ALL {
                    let output_root = run_root
                        .join("output-roots")
                        .join(cache)
                        .join(sample.to_string())
                        .join(sanitize(&scenario.name))
                        .join(adapter.name());
                    tokio::fs::create_dir_all(&output_root).await?;
                    if cache == "warm" {
                        warm_bazel(&config, &worktree, scenario, &output_root, &run_root).await?;
                    }
                    let observation = run_codex(
                        &config,
                        adapter,
                        &worktree,
                        scenario,
                        &output_root,
                        &run_root,
                        &schema_path,
                    )
                    .await
                    .with_context(|| {
                        format!(
                            "Codex live run {cache}/{sample}/{}/{}",
                            scenario.name,
                            adapter.name()
                        )
                    })?;
                    shutdown_bazel(&config, &worktree, &output_root).await;
                    let diagnostic_found = scenario.expected_cause.as_ref().is_none_or(|cause| {
                        observation.verdict.root_cause.contains(cause)
                            || observation.verdict.summary.contains(cause)
                    });
                    samples.push(LiveAgentSample {
                        adapter: adapter.name().to_owned(),
                        scenario: scenario.name.clone(),
                        cache_condition: cache.clone(),
                        sample,
                        expected_exit: scenario.expected_exit,
                        reported_exit: observation.verdict.exit_code,
                        expected_cause: scenario.expected_cause.clone(),
                        diagnostic_found,
                        used_expected_tool: observation.used_expected_tool,
                        end_to_end_ms: observation.end_to_end_ms,
                        model_events: observation.model_events,
                        tool_calls: observation.tool_calls,
                        usage: observation.usage,
                        final_summary: observation.verdict.summary,
                    });
                }
            }
        }
    }
    if !config.keep_worktree {
        remove_worktree(&corpus, &worktree).await?;
    }
    let all_correct = samples.iter().all(|sample| {
        sample.reported_exit == sample.expected_exit
            && sample.diagnostic_found
            && sample.used_expected_tool
    });
    let summaries = summarize(&samples);
    let comparisons = compare_summaries(&summaries)?;
    let report = LiveAgentReport {
        schema_version: 1,
        provider: "codex-cli".to_owned(),
        provider_version: command_version(&config.codex_executable).await,
        model: config.model,
        project: config.project.name,
        commit: config.project.commit,
        bazel_version: config.project.bazel_version,
        environment: environment_metadata(),
        samples,
        summaries,
        comparisons,
    };
    tokio::fs::write(
        run_root.join("live-report.json"),
        serde_json::to_vec_pretty(&report)?,
    )
    .await?;
    tokio::fs::write(run_root.join("live-report.md"), report.markdown()).await?;
    tokio::fs::write(
        config
            .repository_root
            .join(".cache/benchmarks")
            .join(&report.project)
            .join("LATEST_LIVE"),
        format!("live-{run_id}\n"),
    )
    .await?;
    ensure!(
        all_correct,
        "at least one live-agent run was incorrect; inspect {}",
        run_root.join("live-report.json").display()
    );
    Ok(report)
}

async fn run_codex(
    config: &CodexLiveConfig,
    adapter: LiveAdapter,
    worktree: &Path,
    scenario: &Scenario,
    output_root: &Path,
    run_root: &Path,
    schema_path: &Path,
) -> anyhow::Result<CodexObservation> {
    let final_path = output_root.join("agent-final.json");
    let events_path = output_root.join("codex-events.jsonl");
    let stderr_path = output_root.join("codex-stderr.log");
    let stdout = private_file(&events_path)?;
    let stderr = private_file(&stderr_path)?;
    let bazel = bazel_executable()?;
    let server_config_path = if adapter == LiveAdapter::BazelMcp {
        let server = server_executable(config)?;
        let path = output_root.join("server.toml");
        let server_config = ServerConfig {
            allowed_roots: vec![worktree.to_owned()],
            cache_root: output_root.join("mcp-store"),
            bazel_executable: bazel.clone(),
            output_user_root: output_root.to_owned(),
            environment_allowlist: BTreeSet::from(["USE_BAZEL_VERSION".to_owned()]),
        };
        tokio::fs::write(&path, toml::to_string_pretty(&server_config)?).await?;
        Some((server, path))
    } else {
        None
    };

    let mut command = Command::new(&config.codex_executable);
    command
        .arg("exec")
        .args([
            "--json",
            "--ephemeral",
            "--ignore-user-config",
            "--ignore-rules",
        ])
        .args([
            "--sandbox",
            if adapter == LiveAdapter::BazelMcp {
                "workspace-write"
            } else {
                "danger-full-access"
            },
        ])
        .arg("--cd")
        .arg(worktree)
        .arg("--add-dir")
        .arg(output_root)
        .arg("--output-schema")
        .arg(schema_path)
        .arg("--output-last-message")
        .arg(&final_path)
        .args(["--color", "never"])
        .args(["-c", "approval_policy=\"never\""])
        .args(["-c", "shell_environment_policy.inherit=\"all\""])
        .env("USE_BAZEL_VERSION", &config.project.bazel_version)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .kill_on_drop(true);
    if let Some(model) = &config.model {
        command.arg("--model").arg(model);
    }
    if let Some((server, server_config)) = &server_config_path {
        let server_args = vec![
            "--config".to_owned(),
            server_config.to_string_lossy().into_owned(),
            "--log".to_owned(),
            "error".to_owned(),
        ];
        command
            .arg("-c")
            .arg(format!("mcp_servers.bazel.command={}", toml_string(server)))
            .arg("-c")
            .arg(format!(
                "mcp_servers.bazel.args={}",
                serde_json::to_string(&server_args)?
            ))
            .args(["-c", "mcp_servers.bazel.required=true"])
            .args(["-c", "mcp_servers.bazel.tool_timeout_sec=7200"])
            .args(["-c", "mcp_servers.bazel.default_tools_approval_mode=\"approve\""])
            .args(["-c", "mcp_servers.bazel.enabled_tools=[\"bazel.run\",\"bazel.inspect\",\"bazel.cancel\"]"]);
    }
    command.arg(agent_prompt(
        adapter,
        scenario,
        &bazel,
        output_root,
        run_root,
    )?);
    let started = Instant::now();
    let mut child = command.spawn().context("start Codex CLI")?;
    let status = match tokio::time::timeout(config.timeout, child.wait()).await {
        Ok(status) => status?,
        Err(_) => {
            child.kill().await?;
            anyhow::bail!(
                "Codex live-agent run timed out after {} seconds",
                config.timeout.as_secs()
            );
        }
    };
    let end_to_end_ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
    ensure!(
        status.success(),
        "Codex CLI failed; inspect {}",
        stderr_path.display()
    );
    ensure!(
        tokio::fs::metadata(&events_path).await?.len() <= MAX_CODEX_EVENT_BYTES,
        "Codex event stream exceeded {} bytes",
        MAX_CODEX_EVENT_BYTES
    );
    let events = tokio::fs::read_to_string(&events_path).await?;
    let verdict: AgentVerdict = serde_json::from_slice(&tokio::fs::read(&final_path).await?)
        .context("parse Codex schema-constrained final response")?;
    let (usage, model_events, tool_calls, used_expected_tool) =
        parse_codex_events(&events, adapter)?;
    Ok(CodexObservation {
        verdict,
        usage,
        end_to_end_ms,
        model_events,
        tool_calls,
        used_expected_tool,
    })
}

fn parse_codex_events(
    input: &str,
    adapter: LiveAdapter,
) -> anyhow::Result<(ProviderUsage, u64, u64, bool)> {
    let mut usage = ProviderUsage::default();
    let mut model_events = 0_u64;
    let mut tool_calls = 0_u64;
    let mut tool_sequence = Vec::new();
    for (index, line) in input.lines().enumerate() {
        let event: serde_json::Value = serde_json::from_str(line)
            .with_context(|| format!("parse Codex JSONL event {}", index + 1))?;
        let event_type = event.get("type").and_then(serde_json::Value::as_str);
        match event_type {
            Some("turn.completed") => {
                model_events = model_events.saturating_add(1);
                if let Some(value) = event
                    .pointer("/usage/input_tokens")
                    .and_then(serde_json::Value::as_u64)
                {
                    usage.input_tokens = usage.input_tokens.saturating_add(value);
                }
                if let Some(value) = event
                    .pointer("/usage/cached_input_tokens")
                    .and_then(serde_json::Value::as_u64)
                {
                    usage.cached_input_tokens = usage.cached_input_tokens.saturating_add(value);
                }
                if let Some(value) = event
                    .pointer("/usage/output_tokens")
                    .and_then(serde_json::Value::as_u64)
                {
                    usage.output_tokens = usage.output_tokens.saturating_add(value);
                }
                if let Some(value) = event
                    .pointer("/usage/reasoning_output_tokens")
                    .and_then(serde_json::Value::as_u64)
                {
                    usage.reasoning_output_tokens =
                        usage.reasoning_output_tokens.saturating_add(value);
                }
            }
            Some("item.started" | "item.completed") => {
                let item_type = event
                    .pointer("/item/type")
                    .and_then(serde_json::Value::as_str);
                if event_type == Some("item.started")
                    && matches!(item_type, Some("command_execution" | "mcp_tool_call"))
                {
                    tool_calls = tool_calls.saturating_add(1);
                    match item_type {
                        Some("command_execution") => tool_sequence.push("shell".to_owned()),
                        Some("mcp_tool_call") => {
                            let server = event
                                .pointer("/item/server")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or("unknown");
                            let tool = event
                                .pointer("/item/tool")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or("unknown");
                            tool_sequence.push(format!("mcp:{server}:{tool}"));
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    ensure!(
        model_events > 0 && usage.input_tokens > 0,
        "Codex stream has no provider usage"
    );
    let expected = match adapter {
        LiveAdapter::BazelMcp => {
            matches!(
                tool_sequence.as_slice(),
                [run] if run == "mcp:bazel:bazel.run" || run == "mcp:bazel:bazel_run"
            ) || matches!(
                tool_sequence.as_slice(),
                [run, inspect]
                    if (run == "mcp:bazel:bazel.run" || run == "mcp:bazel:bazel_run")
                        && (inspect == "mcp:bazel:bazel.inspect"
                            || inspect == "mcp:bazel:bazel_inspect")
            )
        }
        LiveAdapter::ShellDefault | LiveAdapter::ShellOptimized => {
            tool_sequence.as_slice() == ["shell"]
        }
    };
    Ok((usage, model_events, tool_calls, expected))
}

fn agent_prompt(
    adapter: LiveAdapter,
    scenario: &Scenario,
    bazel: &Path,
    output_root: &Path,
    run_root: &Path,
) -> anyhow::Result<String> {
    let mut arguments = vec![format!("--output_user_root={}", output_root.display())];
    arguments.push(scenario.command.clone());
    arguments.push(format!(
        "--repository_cache={}",
        run_root.join("repository-cache").display()
    ));
    if adapter == LiveAdapter::ShellOptimized {
        arguments.extend(
            ["--color=no", "--curses=no", "--show_progress_rate_limit=60"].map(str::to_owned),
        );
        if scenario.command == "test" {
            arguments.push("--test_output=errors".to_owned());
        }
    }
    arguments.extend(scenario.args.iter().cloned());
    let execution = if adapter == LiveAdapter::BazelMcp {
        format!(
            "Use only the bazel MCP server. Your first tool call must be bazel.run, and you must call it exactly once with command {:?} and args {}. Never call bazel.inspect before bazel.run. Do not invoke Bazel through a shell. Only if bazel.run fails and its result omits the root cause, call bazel.inspect once after it; otherwise make no second tool call.",
            scenario.command,
            serde_json::to_string(&scenario_args(scenario, run_root))?
        )
    } else {
        format!(
            "Use the shell tool and execute exactly one process with executable {} and argv {}. Do not use any MCP tool. {}",
            bazel.display(),
            serde_json::to_string(&arguments)?,
            if adapter == LiveAdapter::ShellOptimized {
                "Wait at least 30 seconds before the first poll and then poll no more often than every 60 seconds. Read a failed test log at most once."
            } else {
                "Use the host's normal polling behavior."
            }
        )
    };
    Ok(format!(
        "This is a controlled benchmark in a disposable Abseil worktree. Do not edit source files. {execution} Report the real Bazel exit code. On failure, identify the actionable root cause{} and preserve its exact marker in root_cause or summary. Return only the requested schema.",
        scenario
            .expected_cause
            .as_ref()
            .map_or(String::new(), |cause| format!(" containing {cause:?}"))
    ))
}

async fn warm_bazel(
    config: &CodexLiveConfig,
    worktree: &Path,
    scenario: &Scenario,
    output_root: &Path,
    run_root: &Path,
) -> anyhow::Result<()> {
    let status = Command::new(bazel_executable()?)
        .current_dir(worktree)
        .env("USE_BAZEL_VERSION", &config.project.bazel_version)
        .arg(format!("--output_user_root={}", output_root.display()))
        .arg(&scenario.command)
        .arg(format!(
            "--repository_cache={}",
            run_root.join("repository-cache").display()
        ))
        .args(&scenario.args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await?;
    ensure!(
        status.code() == Some(scenario.expected_exit),
        "live-agent warmup exit mismatch"
    );
    Ok(())
}

async fn shutdown_bazel(config: &CodexLiveConfig, worktree: &Path, output_root: &Path) {
    let Ok(bazel) = bazel_executable() else {
        return;
    };
    let _ = Command::new(bazel)
        .current_dir(worktree)
        .env("USE_BAZEL_VERSION", &config.project.bazel_version)
        .arg(format!("--output_user_root={}", output_root.display()))
        .arg("shutdown")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

fn summarize(samples: &[LiveAgentSample]) -> Vec<LiveAgentSummary> {
    LiveAdapter::ALL
        .iter()
        .map(|adapter| {
            let matching: Vec<_> = samples
                .iter()
                .filter(|sample| sample.adapter == adapter.name())
                .collect();
            LiveAgentSummary {
                adapter: adapter.name().to_owned(),
                observations: matching.len(),
                correct_observations: matching
                    .iter()
                    .filter(|sample| {
                        sample.reported_exit == sample.expected_exit
                            && sample.diagnostic_found
                            && sample.used_expected_tool
                    })
                    .count(),
                input_tokens: matching
                    .iter()
                    .map(|sample| sample.usage.input_tokens)
                    .sum(),
                cached_input_tokens: matching
                    .iter()
                    .map(|sample| sample.usage.cached_input_tokens)
                    .sum(),
                uncached_input_tokens: matching
                    .iter()
                    .map(|sample| sample.usage.uncached_input_tokens())
                    .sum(),
                output_tokens: matching
                    .iter()
                    .map(|sample| sample.usage.output_tokens)
                    .sum(),
                reasoning_output_tokens: matching
                    .iter()
                    .map(|sample| sample.usage.reasoning_output_tokens)
                    .sum(),
                total_tokens: matching
                    .iter()
                    .map(|sample| sample.usage.total_tokens())
                    .sum(),
            }
        })
        .collect()
}

fn compare_summaries(summaries: &[LiveAgentSummary]) -> anyhow::Result<Vec<LiveAgentComparison>> {
    let candidate = summaries
        .iter()
        .find(|summary| summary.adapter == LiveAdapter::BazelMcp.name())
        .context("MCP live summary is missing")?;
    [LiveAdapter::ShellDefault, LiveAdapter::ShellOptimized]
        .iter()
        .map(|baseline| {
            let baseline = summaries
                .iter()
                .find(|summary| summary.adapter == baseline.name())
                .context("shell live summary is missing")?;
            Ok(LiveAgentComparison {
                baseline_adapter: baseline.adapter.clone(),
                candidate_adapter: candidate.adapter.clone(),
                input_token_reduction_percent: reduction(
                    candidate.input_tokens,
                    baseline.input_tokens,
                ),
                uncached_input_token_reduction_percent: reduction(
                    candidate.uncached_input_tokens,
                    baseline.uncached_input_tokens,
                ),
                total_token_reduction_percent: reduction(
                    candidate.total_tokens,
                    baseline.total_tokens,
                ),
            })
        })
        .collect()
}

fn reduction(candidate: u64, baseline: u64) -> f64 {
    if baseline == 0 {
        0.0
    } else {
        100.0 * (1.0 - candidate as f64 / baseline as f64)
    }
}

fn scenario_args(scenario: &Scenario, run_root: &Path) -> Vec<String> {
    let mut arguments = vec![format!(
        "--repository_cache={}",
        run_root.join("repository-cache").display()
    )];
    arguments.extend(scenario.args.iter().cloned());
    arguments
}

fn private_file(path: &Path) -> anyhow::Result<std::fs::File> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .with_context(|| format!("create private capture {}", path.display()))
}

fn server_executable(config: &CodexLiveConfig) -> anyhow::Result<PathBuf> {
    let path = std::env::var_os("BAZEL_MCP_SERVER_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| config.repository_root.join("target/debug/bazel-mcp"));
    ensure!(
        path.is_file(),
        "bazel-mcp server is missing at {}",
        path.display()
    );
    Ok(path)
}

fn bazel_executable() -> anyhow::Result<PathBuf> {
    if let Some(path) = std::env::var_os("BAZEL_MCP_BAZEL") {
        return Ok(PathBuf::from(path));
    }
    which_on_path("bazelisk")
        .or_else(|| which_on_path("bazel"))
        .context("bazelisk or bazel is required")
}

fn which_on_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

fn toml_string(path: &Path) -> String {
    serde_json::to_string(&path.to_string_lossy()).expect("paths serialize as JSON strings")
}

fn verdict_schema() -> &'static [u8] {
    br#"{"type":"object","properties":{"exit_code":{"type":"integer"},"root_cause":{"type":"string"},"summary":{"type":"string"}},"required":["exit_code","root_cause","summary"],"additionalProperties":false}"#
}

async fn command_version(executable: &Path) -> String {
    Command::new(executable)
        .arg("--version")
        .output()
        .await
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_else(|| "unknown".to_owned())
}

fn environment_metadata() -> EnvironmentMetadata {
    let rustc = std::process::Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_else(|| "unknown".to_owned());
    EnvironmentMetadata {
        os: std::env::consts::OS.to_owned(),
        architecture: std::env::consts::ARCH.to_owned(),
        rustc,
        logical_cpus: std::thread::available_parallelism().map_or(1, usize::from),
    }
}

async fn create_worktree(corpus: &Path, worktree: &Path, commit: &str) -> anyhow::Result<()> {
    let status = Command::new("git")
        .args(["-C"])
        .arg(corpus)
        .args(["worktree", "add", "--detach", "--force"])
        .arg(worktree)
        .arg(commit)
        .status()
        .await?;
    ensure!(status.success(), "could not create live benchmark worktree");
    Ok(())
}

async fn remove_worktree(corpus: &Path, worktree: &Path) -> anyhow::Result<()> {
    let status = Command::new("git")
        .args(["-C"])
        .arg(corpus)
        .args(["worktree", "remove", "--force"])
        .arg(worktree)
        .status()
        .await?;
    ensure!(status.success(), "could not remove live benchmark worktree");
    Ok(())
}

async fn install_overlay(repository_root: &Path, worktree: &Path) -> anyhow::Result<()> {
    let source = repository_root.join("crates/bazel-mcp-benchmark/resources/scenarios/abseil-cpp");
    let destination = worktree.join("bazel_mcp_token_bench");
    tokio::task::spawn_blocking(move || copy_directory(&source, &destination)).await??;
    Ok(())
}

fn copy_directory(source: &Path, destination: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_directory(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_codex_usage_and_enforces_adapter_tool() {
        let events = r#"{"type":"item.started","item":{"type":"mcp_tool_call","server":"bazel","tool":"bazel.run"}}
{"type":"turn.completed","usage":{"input_tokens":100,"cached_input_tokens":60,"output_tokens":20,"reasoning_output_tokens":3}}
"#;
        let (usage, turns, calls, expected) =
            parse_codex_events(events, LiveAdapter::BazelMcp).unwrap();
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.uncached_input_tokens(), 40);
        assert_eq!(usage.total_tokens(), 120);
        assert_eq!(turns, 1);
        assert_eq!(calls, 1);
        assert!(expected);
        assert!(
            !parse_codex_events(events, LiveAdapter::ShellDefault)
                .unwrap()
                .3
        );

        let inspect_first = r#"{"type":"item.started","item":{"type":"mcp_tool_call","server":"bazel","tool":"bazel.inspect"}}
{"type":"item.started","item":{"type":"mcp_tool_call","server":"bazel","tool":"bazel.run"}}
{"type":"turn.completed","usage":{"input_tokens":100,"output_tokens":20}}
"#;
        assert!(
            !parse_codex_events(inspect_first, LiveAdapter::BazelMcp)
                .unwrap()
                .3
        );
    }

    #[test]
    fn summary_comparison_uses_provider_reported_tokens() {
        let summaries = vec![
            LiveAgentSummary {
                adapter: "shell-default".into(),
                observations: 1,
                correct_observations: 1,
                input_tokens: 1_000,
                cached_input_tokens: 0,
                uncached_input_tokens: 1_000,
                output_tokens: 100,
                reasoning_output_tokens: 10,
                total_tokens: 1_100,
            },
            LiveAgentSummary {
                adapter: "shell-optimized".into(),
                observations: 1,
                correct_observations: 1,
                input_tokens: 800,
                cached_input_tokens: 0,
                uncached_input_tokens: 800,
                output_tokens: 80,
                reasoning_output_tokens: 8,
                total_tokens: 880,
            },
            LiveAgentSummary {
                adapter: "bazel-mcp".into(),
                observations: 1,
                correct_observations: 1,
                input_tokens: 200,
                cached_input_tokens: 0,
                uncached_input_tokens: 200,
                output_tokens: 20,
                reasoning_output_tokens: 2,
                total_tokens: 220,
            },
        ];
        let comparisons = compare_summaries(&summaries).unwrap();
        assert_eq!(comparisons.len(), 2);
        assert!((comparisons[0].input_token_reduction_percent - 80.0).abs() < f64::EPSILON);
        assert!((comparisons[1].total_token_reduction_percent - 75.0).abs() < f64::EPSILON);
    }
}
