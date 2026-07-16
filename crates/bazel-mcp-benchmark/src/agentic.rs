use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs::OpenOptions,
    io::Write,
    path::{Component, Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context, ensure};
use bazel_mcp_policy::Redactor;
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::process::Command;

use crate::{EnvironmentMetadata, Estimate, ProviderUsage};

const BOOTSTRAP_RESAMPLES: usize = 4_000;
const CACHE_WEIGHT_PERCENTAGES: [u32; 3] = [0, 25, 100];
const MAX_CODEX_EVENT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_PATCH_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
#[value(rename_all = "kebab-case")]
pub enum AgenticAdapter {
    ShellDefault,
    ShellOptimized,
    ShellMcpLoaded,
    BazelMcp,
}

impl AgenticAdapter {
    pub const fn name(self) -> &'static str {
        match self {
            Self::ShellDefault => "shell-default",
            Self::ShellOptimized => "shell-optimized",
            Self::ShellMcpLoaded => "shell-mcp-loaded",
            Self::BazelMcp => "bazel-mcp",
        }
    }

    const fn loads_mcp(self) -> bool {
        matches!(self, Self::ShellMcpLoaded | Self::BazelMcp)
    }

    const fn uses_shell_bazel(self) -> bool {
        !matches!(self, Self::BazelMcp)
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct AgenticProjectManifest {
    pub name: String,
    pub url: String,
    pub commit: String,
    pub license: String,
    pub bazel_version: String,
    pub tasks: Vec<AgenticTask>,
    #[serde(skip)]
    resource_root: PathBuf,
}

#[derive(Clone, Debug, Deserialize)]
pub struct AgenticTask {
    pub name: String,
    pub prompt_file: PathBuf,
    pub workspace_overlay: PathBuf,
    pub verification_overlay: Option<PathBuf>,
    pub verify_command: String,
    pub verify_args: Vec<String>,
    #[serde(default)]
    pub protected_paths: Vec<PathBuf>,
    #[serde(default = "default_task_timeout_seconds")]
    pub timeout_seconds: u64,
}

fn default_task_timeout_seconds() -> u64 {
    1_800
}

impl AgenticProjectManifest {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let source = std::fs::read_to_string(path)
            .with_context(|| format!("read agentic manifest {}", path.display()))?;
        let mut manifest: Self = toml::from_str(&source)
            .with_context(|| format!("parse agentic manifest {}", path.display()))?;
        manifest.resource_root = path
            .parent()
            .context("agentic manifest has no resource directory")?
            .to_owned();
        manifest.validate()?;
        Ok(manifest)
    }

    fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            self.commit.len() == 40 && self.commit.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "agentic corpus commit must be a full 40-character SHA-1"
        );
        ensure!(
            !self.license.is_empty(),
            "agentic corpus license is missing"
        );
        ensure!(!self.tasks.is_empty(), "agentic manifest has no tasks");
        let mut names = BTreeSet::new();
        for task in &self.tasks {
            ensure!(
                !task.name.is_empty() && names.insert(task.name.clone()),
                "agentic task names must be nonempty and unique"
            );
            ensure!(
                matches!(task.verify_command.as_str(), "build" | "test"),
                "agentic verifier command must be build or test"
            );
            ensure!(
                task.timeout_seconds > 0,
                "agentic task timeout must be positive"
            );
            validate_relative_path(&task.prompt_file)?;
            validate_relative_path(&task.workspace_overlay)?;
            ensure!(
                self.resource_path(&task.prompt_file).is_file(),
                "agentic prompt is missing: {}",
                task.prompt_file.display()
            );
            ensure!(
                self.resource_path(&task.workspace_overlay).is_dir(),
                "agentic workspace overlay is missing: {}",
                task.workspace_overlay.display()
            );
            if let Some(path) = &task.verification_overlay {
                validate_relative_path(path)?;
                ensure!(
                    self.resource_path(path).is_dir(),
                    "agentic verification overlay is missing: {}",
                    path.display()
                );
            }
            for protected in &task.protected_paths {
                validate_relative_path(protected)?;
                ensure!(
                    self.resource_path(&task.workspace_overlay)
                        .join(protected)
                        .is_file(),
                    "protected path is missing from task overlay: {}",
                    protected.display()
                );
            }
        }
        Ok(())
    }

    fn resource_path(&self, path: &Path) -> PathBuf {
        self.resource_root.join(path)
    }

    fn prompt(&self, task: &AgenticTask) -> anyhow::Result<String> {
        std::fs::read_to_string(self.resource_path(&task.prompt_file))
            .with_context(|| format!("read agentic prompt {}", task.prompt_file.display()))
    }
}

#[derive(Clone, Debug)]
pub struct AgenticConfig {
    pub repository_root: PathBuf,
    pub project: AgenticProjectManifest,
    pub samples: u32,
    pub task_filter: BTreeSet<String>,
    pub adapters: Vec<AgenticAdapter>,
    pub keep_worktrees: bool,
    pub codex_executable: PathBuf,
    pub proxy_executable: PathBuf,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub timeout_override: Option<Duration>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgenticSample {
    pub adapter: String,
    pub task: String,
    pub sample: u32,
    pub verified: bool,
    pub verifier_exit_code: Option<i32>,
    pub protected_paths_unchanged: bool,
    pub protected_path_violations: Vec<String>,
    pub used_expected_bazel_path: bool,
    pub shell_bazel_calls: u64,
    pub mcp_bazel_run_calls: u64,
    pub model_events: u64,
    pub agent_message_events: u64,
    pub tool_calls: u64,
    pub file_change_events: u64,
    pub command_output_bytes: u64,
    pub mcp_output_bytes: u64,
    pub changed_paths: Vec<String>,
    pub patch_bytes: u64,
    pub end_to_end_ms: u64,
    pub verifier_ms: u64,
    pub usage: ProviderUsage,
    pub final_summary: String,
    pub reported_validation: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgenticSummary {
    pub adapter: String,
    pub attempts: usize,
    pub verified_solves: usize,
    pub solve_rate_percent: f64,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub uncached_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    pub total_tokens: u64,
    pub tokens_per_verified_solve: Option<f64>,
    pub agent_message_events: u64,
    pub tool_calls: u64,
    pub command_output_bytes: u64,
    pub mcp_output_bytes: u64,
    pub end_to_end_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgenticWeightedSummary {
    pub adapter: String,
    pub cached_input_weight_percent: u32,
    pub weighted_tokens: f64,
    pub weighted_tokens_per_verified_solve: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgenticComparison {
    pub baseline_adapter: String,
    pub candidate_adapter: String,
    pub paired_attempts: usize,
    pub concordant_verified_solves: usize,
    pub solve_rate_delta_percentage_points: f64,
    pub total_token_reduction_percent: Estimate,
    pub uncached_input_reduction_percent: Estimate,
    pub concordant_total_token_reduction_percent: Option<Estimate>,
    pub tokens_per_verified_solve_reduction_percent: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgenticWeightedComparison {
    pub baseline_adapter: String,
    pub candidate_adapter: String,
    pub cached_input_weight_percent: u32,
    pub weighted_token_reduction_percent: Estimate,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgenticTaskComparison {
    pub task: String,
    pub baseline_adapter: String,
    pub candidate_adapter: String,
    pub paired_attempts: usize,
    pub baseline_verified_solves: usize,
    pub candidate_verified_solves: usize,
    pub total_token_reduction_percent: f64,
    pub active_token_reduction_percent: f64,
    pub tool_output_byte_reduction_percent: f64,
    pub end_to_end_reduction_percent: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgenticReport {
    pub schema_version: u32,
    pub provider: String,
    pub provider_version: String,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub project: String,
    pub commit: String,
    pub bazel_version: String,
    pub adapter_order_seed: u64,
    pub adapter_order_strategy: String,
    pub environment: EnvironmentMetadata,
    pub samples: Vec<AgenticSample>,
    pub summaries: Vec<AgenticSummary>,
    pub comparisons: Vec<AgenticComparison>,
    pub task_comparisons: Vec<AgenticTaskComparison>,
    pub weighted_summaries: Vec<AgenticWeightedSummary>,
    pub weighted_comparisons: Vec<AgenticWeightedComparison>,
}

impl AgenticReport {
    pub fn markdown(&self) -> String {
        let mut output = format!(
            "# Bazel MCP agentic coding benchmark\n\nProvider: `{}` (`{}`)  \nModel: `{}`  \nReasoning effort: `{}`  \nProject: `{}` @ `{}`  \nBazel: `{}`\n\n",
            self.provider,
            self.provider_version,
            self.model.as_deref().unwrap_or("provider default"),
            self.reasoning_effort
                .as_deref()
                .unwrap_or("provider default"),
            self.project,
            self.commit,
            self.bazel_version,
        );
        output.push_str("## Outcomes and usage\n\n");
        output.push_str("| Adapter | Attempts | Verified | Solve rate | Input | Cached input | Uncached input | Output | Total | Tokens / verified solve |\n");
        output.push_str("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n");
        for summary in &self.summaries {
            output.push_str(&format!(
                "| {} | {} | {} | {:.2}% | {} | {} | {} | {} | {} | {} |\n",
                summary.adapter,
                summary.attempts,
                summary.verified_solves,
                summary.solve_rate_percent,
                summary.input_tokens,
                summary.cached_input_tokens,
                summary.uncached_input_tokens,
                summary.output_tokens,
                summary.total_tokens,
                optional_number(summary.tokens_per_verified_solve),
            ));
        }
        output.push_str("\n## Task-level paired deltas\n\n");
        output.push_str("Positive reductions mean MCP used less than the named baseline. Active tokens count uncached input plus output; tool output combines shell-command and MCP-result bytes.\n\n");
        output.push_str("| Task | Baseline | Pairs | Verified (base/MCP) | Total-token reduction | Active-token reduction | Tool-output reduction | Agent-time reduction |\n");
        output.push_str("| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |\n");
        for comparison in &self.task_comparisons {
            output.push_str(&format!(
                "| {} | {} | {} | {}/{} | {:.2}% | {:.2}% | {:.2}% | {:.2}% |\n",
                comparison.task,
                comparison.baseline_adapter,
                comparison.paired_attempts,
                comparison.baseline_verified_solves,
                comparison.candidate_verified_solves,
                comparison.total_token_reduction_percent,
                comparison.active_token_reduction_percent,
                comparison.tool_output_byte_reduction_percent,
                comparison.end_to_end_reduction_percent,
            ));
        }
        output.push_str("\n## Agent behavior and evidence\n\n");
        output.push_str("| Adapter | Agent messages | Tool calls | Command output bytes | MCP output bytes | Agent time |\n");
        output.push_str("| --- | ---: | ---: | ---: | ---: | ---: |\n");
        for summary in &self.summaries {
            output.push_str(&format!(
                "| {} | {} | {} | {} | {} | {:.1}s |\n",
                summary.adapter,
                summary.agent_message_events,
                summary.tool_calls,
                summary.command_output_bytes,
                summary.mcp_output_bytes,
                summary.end_to_end_ms as f64 / 1_000.0,
            ));
        }
        output.push_str("\n## Paired comparisons\n\n");
        output.push_str("Token intervals use deterministic task-clustered bootstrap resampling. A token-savings claim is valid only when solve-rate parity is acceptable.\n\n");
        output.push_str("| Baseline | Pairs | Concordant solves | Solve-rate delta | Total-token reduction | Uncached-input reduction | Concordant-solve reduction | Tokens/solve reduction |\n");
        output.push_str("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n");
        for comparison in &self.comparisons {
            output.push_str(&format!(
                "| {} | {} | {} | {:.2} pp | {} | {} | {} | {} |\n",
                comparison.baseline_adapter,
                comparison.paired_attempts,
                comparison.concordant_verified_solves,
                comparison.solve_rate_delta_percentage_points,
                format_estimate(&comparison.total_token_reduction_percent),
                format_estimate(&comparison.uncached_input_reduction_percent),
                comparison
                    .concordant_total_token_reduction_percent
                    .as_ref()
                    .map_or_else(|| "n/a".to_owned(), format_estimate),
                optional_percent(comparison.tokens_per_verified_solve_reduction_percent),
            ));
        }
        output.push_str("\n## Cached-input sensitivity\n\n");
        output.push_str("Weighted tokens count uncached input and output at 100%, then vary the weight assigned to cached input. These are sensitivity scenarios, not pricing assumptions.\n\n");
        output.push_str("| Adapter | Cached-input weight | Weighted tokens | Weighted tokens / verified solve |\n");
        output.push_str("| --- | ---: | ---: | ---: |\n");
        for summary in &self.weighted_summaries {
            output.push_str(&format!(
                "| {} | {}% | {:.0} | {} |\n",
                summary.adapter,
                summary.cached_input_weight_percent,
                summary.weighted_tokens,
                optional_number(summary.weighted_tokens_per_verified_solve),
            ));
        }
        output.push_str("\n| Baseline | Cached-input weight | Weighted-token reduction |\n");
        output.push_str("| --- | ---: | ---: |\n");
        for comparison in &self.weighted_comparisons {
            output.push_str(&format!(
                "| {} | {}% | {} |\n",
                comparison.baseline_adapter,
                comparison.cached_input_weight_percent,
                format_estimate(&comparison.weighted_token_reduction_percent),
            ));
        }
        output
    }
}

#[derive(Debug, Deserialize)]
struct AgentVerdict {
    summary: String,
    validation: Vec<String>,
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
    agent_message_events: u64,
    tool_calls: u64,
    file_change_events: u64,
    command_output_bytes: u64,
    mcp_output_bytes: u64,
    shell_bazel_calls: u64,
    mcp_bazel_run_calls: u64,
    used_expected_bazel_path: bool,
}

#[derive(Debug)]
struct ParsedEvents {
    usage: ProviderUsage,
    model_events: u64,
    agent_message_events: u64,
    tool_calls: u64,
    file_change_events: u64,
    command_output_bytes: u64,
    mcp_output_bytes: u64,
    mcp_bazel_run_calls: u64,
}

#[derive(Debug)]
struct Snapshot {
    worktree: PathBuf,
    base_commit: String,
    protected_hashes: BTreeMap<String, String>,
}

pub async fn run_agentic_benchmark(mut config: AgenticConfig) -> anyhow::Result<AgenticReport> {
    ensure!(
        config.samples > 0,
        "agentic samples must be greater than zero"
    );
    ensure!(
        !config.adapters.is_empty(),
        "select at least one agentic adapter"
    );
    ensure!(
        config.codex_executable.is_file(),
        "Codex executable is missing"
    );
    ensure!(
        config.proxy_executable.is_file(),
        "agentic Bazel proxy is missing"
    );
    let mut adapter_names = BTreeSet::new();
    ensure!(
        config
            .adapters
            .iter()
            .all(|adapter| adapter_names.insert(adapter.name())),
        "agentic adapters must be unique"
    );
    let mut server = if config.adapters.iter().any(|adapter| adapter.loads_mcp()) {
        Some(server_executable(&config.repository_root)?)
    } else {
        None
    };
    let bazel = bazel_executable()?;
    let corpus = config
        .repository_root
        .join(".cache/corpora")
        .join(&config.project.name)
        .join(&config.project.commit);
    ensure!(
        corpus.join(".git").exists(),
        "corpus is missing; run make setup-oss-corpus"
    );

    let run_id = SystemTime::UNIX_EPOCH.elapsed()?.as_millis().to_string();
    let adapter_order_seed = run_id.parse::<u64>()?;
    let run_root = config
        .repository_root
        .join(".cache/benchmarks")
        .join(&config.project.name)
        .join(format!("agentic-{run_id}"));
    tokio::fs::create_dir_all(&run_root).await?;
    let binary_root = run_root.join("harness-binaries");
    config.proxy_executable = snapshot_executable(
        &config.proxy_executable,
        &binary_root.join("bazel-agentic-shell"),
    )
    .await?;
    if let Some(executable) = server.as_deref() {
        server = Some(snapshot_executable(executable, &binary_root.join("bazel-mcp")).await?);
    }
    let schema_path = run_root.join("agent-result.schema.json");
    tokio::fs::write(&schema_path, verdict_schema()).await?;

    let tasks: Vec<_> = config
        .project
        .tasks
        .iter()
        .filter(|task| config.task_filter.is_empty() || config.task_filter.contains(&task.name))
        .cloned()
        .collect();
    ensure!(!tasks.is_empty(), "no agentic tasks selected");

    let repository_cache = run_root.join("repository-cache");
    tokio::fs::create_dir_all(&repository_cache).await?;
    let mut samples = Vec::new();
    for sample in 0..config.samples {
        for (task_index, task) in tasks.iter().enumerate() {
            let mut adapters = config.adapters.clone();
            let rotation = (sample as usize + task_index) % adapters.len();
            adapters.rotate_left(rotation);
            for adapter in adapters {
                let artifact = format!("{}-{}-{}", sample, sanitize(&task.name), adapter.name());
                let snapshot_path = run_root.join("worktrees").join(&artifact);
                let output_root = run_root.join("attempts").join(&artifact);
                tokio::fs::create_dir_all(&output_root).await?;
                let snapshot =
                    create_snapshot(&config, task, &corpus, &snapshot_path, &repository_cache)
                        .await?;
                let attempt = run_attempt(
                    &config,
                    task,
                    sample,
                    adapter,
                    &snapshot,
                    &output_root,
                    &repository_cache,
                    &schema_path,
                    &bazel,
                    server.as_deref(),
                )
                .await;
                if !config.keep_worktrees && attempt.is_ok() {
                    remove_worktree(&corpus, &snapshot.worktree).await?;
                }
                samples.push(attempt?);
            }
        }
    }

    let summaries = summarize(&samples, &config.adapters);
    let comparisons = compare(&samples, &summaries, &config.adapters)?;
    let task_comparisons = compare_tasks(&samples, &config.adapters)?;
    let weighted_summaries = summarize_weighted(&samples, &config.adapters);
    let weighted_comparisons = compare_weighted(&samples, &config.adapters)?;
    let report = AgenticReport {
        schema_version: 3,
        provider: "codex-cli".to_owned(),
        provider_version: command_version(&config.codex_executable).await,
        model: config.model,
        reasoning_effort: config.reasoning_effort,
        project: config.project.name.clone(),
        commit: config.project.commit.clone(),
        bazel_version: config.project.bazel_version.clone(),
        adapter_order_seed,
        adapter_order_strategy: "cyclic-task-counterbalanced-v1".to_owned(),
        environment: environment_metadata(),
        samples,
        summaries,
        comparisons,
        task_comparisons,
        weighted_summaries,
        weighted_comparisons,
    };
    tokio::fs::write(
        run_root.join("agentic-report.json"),
        serde_json::to_vec_pretty(&report)?,
    )
    .await?;
    tokio::fs::write(run_root.join("agentic-report.md"), report.markdown()).await?;
    tokio::fs::write(
        config
            .repository_root
            .join(".cache/benchmarks")
            .join(&report.project)
            .join("LATEST_AGENTIC"),
        format!("agentic-{run_id}\n"),
    )
    .await?;
    Ok(report)
}

#[allow(clippy::too_many_arguments)]
async fn run_attempt(
    config: &AgenticConfig,
    task: &AgenticTask,
    sample: u32,
    adapter: AgenticAdapter,
    snapshot: &Snapshot,
    output_root: &Path,
    repository_cache: &Path,
    schema_path: &Path,
    bazel: &Path,
    server: Option<&Path>,
) -> anyhow::Result<AgenticSample> {
    let observation = run_codex(
        config,
        task,
        adapter,
        &snapshot.worktree,
        output_root,
        schema_path,
        bazel,
        server,
    )
    .await?;
    let agent_bazel_root = output_root.join(if adapter.uses_shell_bazel() {
        "shell-output-user-root"
    } else {
        "mcp-output-user-root"
    });
    if agent_bazel_root.is_dir() {
        shutdown_bazel(config, &snapshot.worktree, &agent_bazel_root, bazel).await;
    }
    let protected_path_violations = protected_path_violations(snapshot)?;
    let protected_paths_unchanged = protected_path_violations.is_empty();
    let (patch, changed_paths) = capture_patch(&snapshot.worktree, &snapshot.base_commit).await?;
    write_private(&output_root.join("patch.diff"), &patch)?;
    if let Some(overlay) = &task.verification_overlay {
        let source = config.project.resource_path(overlay);
        let destination = snapshot.worktree.clone();
        tokio::task::spawn_blocking(move || copy_directory(&source, &destination, false)).await??;
    }
    let verifier_root = output_root.join("verifier-output-root");
    let (verifier_exit_code, verifier_ms) = run_verifier(
        config,
        task,
        &snapshot.worktree,
        output_root,
        repository_cache,
        &verifier_root,
        bazel,
    )
    .await?;
    let patch_bytes = patch.len() as u64;
    let verified = verifier_exit_code == Some(0)
        && protected_paths_unchanged
        && observation.used_expected_bazel_path
        && patch_bytes > 0;
    let final_summary = redact_report_text(&canonicalize_local_paths(
        &observation.verdict.summary,
        &snapshot.worktree,
        output_root,
    ));
    let reported_validation = observation
        .verdict
        .validation
        .iter()
        .map(|value| {
            redact_report_text(&canonicalize_local_paths(
                value,
                &snapshot.worktree,
                output_root,
            ))
        })
        .collect();
    Ok(AgenticSample {
        adapter: adapter.name().to_owned(),
        task: task.name.clone(),
        sample,
        verified,
        verifier_exit_code,
        protected_paths_unchanged,
        protected_path_violations,
        used_expected_bazel_path: observation.used_expected_bazel_path,
        shell_bazel_calls: observation.shell_bazel_calls,
        mcp_bazel_run_calls: observation.mcp_bazel_run_calls,
        model_events: observation.model_events,
        agent_message_events: observation.agent_message_events,
        tool_calls: observation.tool_calls,
        file_change_events: observation.file_change_events,
        command_output_bytes: observation.command_output_bytes,
        mcp_output_bytes: observation.mcp_output_bytes,
        changed_paths,
        patch_bytes,
        end_to_end_ms: observation.end_to_end_ms,
        verifier_ms,
        usage: observation.usage,
        final_summary,
        reported_validation,
    })
}

#[allow(clippy::too_many_arguments)]
async fn run_codex(
    config: &AgenticConfig,
    task: &AgenticTask,
    adapter: AgenticAdapter,
    worktree: &Path,
    output_root: &Path,
    schema_path: &Path,
    bazel: &Path,
    server: Option<&Path>,
) -> anyhow::Result<CodexObservation> {
    let final_path = output_root.join("agent-final.json");
    let events_path = output_root.join("codex-events.jsonl");
    let stderr_path = output_root.join("codex-stderr.log");
    let wrapper_log = output_root.join("shell-bazel-calls.jsonl");
    let stdout = private_file(&events_path)?;
    let stderr = private_file(&stderr_path)?;
    let wrapper_dir = output_root.join("bin");
    install_proxy(&config.proxy_executable, &wrapper_dir).await?;
    let path = benchmark_path(&wrapper_dir)?;

    let server_config_path = if adapter.loads_mcp() {
        let server = server.context("bazel-mcp server is required for the MCP adapter")?;
        let path = output_root.join("server.toml");
        let server_config = ServerConfig {
            allowed_roots: vec![worktree.to_owned()],
            cache_root: output_root.join("mcp-store"),
            bazel_executable: bazel.to_owned(),
            output_user_root: output_root.join("mcp-output-user-root"),
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
            "--sandbox",
            // Bazel's persistent server binds a loopback socket and inspects
            // child processes. The macOS workspace-write sandbox blocks both,
            // which makes the shell baseline fail before exercising the task.
            // Use identical permissions for every adapter; isolation comes
            // from the disposable worktree and per-attempt Bazel output root.
            "danger-full-access",
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
        .env("PATH", path)
        .env("USE_BAZEL_VERSION", &config.project.bazel_version)
        .env("BAZEL_AGENTIC_LOG", &wrapper_log)
        .env(
            "BAZEL_AGENTIC_OUTPUT_USER_ROOT",
            output_root.join("shell-output-user-root"),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .kill_on_drop(true);
    if adapter.uses_shell_bazel() {
        command
            .env("BAZEL_AGENTIC_DENY", "0")
            .env("BAZEL_AGENTIC_REAL", bazel);
    } else {
        command
            .env("BAZEL_AGENTIC_DENY", "1")
            .env_remove("BAZEL_AGENTIC_REAL");
    }
    if let Some(model) = &config.model {
        command.arg("--model").arg(model);
    }
    if let Some(effort) = &config.reasoning_effort {
        command
            .arg("-c")
            .arg(format!("model_reasoning_effort={}", toml_text(effort)));
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
            .arg(format!("mcp_servers.bazel.command={}", toml_path(server)))
            .arg("-c")
            .arg(format!(
                "mcp_servers.bazel.args={}",
                serde_json::to_string(&server_args)?
            ))
            .arg("-c")
            .arg(format!(
                "mcp_servers.bazel.env={{USE_BAZEL_VERSION={}}}",
                toml_text(&config.project.bazel_version)
            ))
            .args(["-c", "mcp_servers.bazel.required=true"])
            .args(["-c", "mcp_servers.bazel.tool_timeout_sec=7200"])
            .args([
                "-c",
                "mcp_servers.bazel.default_tools_approval_mode=\"approve\"",
            ])
            .args([
                "-c",
                "mcp_servers.bazel.enabled_tools=[\"bazel.run\",\"bazel.inspect\",\"bazel.cancel\"]",
            ]);
    }
    command.arg(agent_prompt(config, task, adapter)?);

    let started = Instant::now();
    let mut child = command.spawn().context("start Codex agentic benchmark")?;
    let timeout = config
        .timeout_override
        .unwrap_or_else(|| Duration::from_secs(task.timeout_seconds));
    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(status) => status?,
        Err(_) => {
            child.kill().await?;
            anyhow::bail!(
                "Codex agentic run timed out after {} seconds; inspect {}",
                timeout.as_secs(),
                stderr_path.display()
            );
        }
    };
    let end_to_end_ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
    ensure!(
        status.success(),
        "Codex agentic run failed; inspect {}",
        stderr_path.display()
    );
    ensure!(
        tokio::fs::metadata(&events_path).await?.len() <= MAX_CODEX_EVENT_BYTES,
        "Codex event stream exceeded {} bytes",
        MAX_CODEX_EVENT_BYTES
    );
    let events = tokio::fs::read_to_string(&events_path).await?;
    let parsed = parse_codex_events(&events)?;
    ensure!(
        parsed.model_events > 0 && parsed.usage.input_tokens > 0,
        "Codex event stream has no provider usage"
    );
    let verdict: AgentVerdict = serde_json::from_slice(&tokio::fs::read(&final_path).await?)
        .context("parse Codex agentic final response")?;
    let shell_bazel_calls = count_wrapper_calls(&wrapper_log)?;
    let used_expected_bazel_path = match adapter {
        AgenticAdapter::BazelMcp => parsed.mcp_bazel_run_calls > 0 && shell_bazel_calls == 0,
        AgenticAdapter::ShellDefault
        | AgenticAdapter::ShellOptimized
        | AgenticAdapter::ShellMcpLoaded => {
            shell_bazel_calls > 0 && parsed.mcp_bazel_run_calls == 0
        }
    };
    Ok(CodexObservation {
        verdict,
        usage: parsed.usage,
        end_to_end_ms,
        model_events: parsed.model_events,
        agent_message_events: parsed.agent_message_events,
        tool_calls: parsed.tool_calls,
        file_change_events: parsed.file_change_events,
        command_output_bytes: parsed.command_output_bytes,
        mcp_output_bytes: parsed.mcp_output_bytes,
        shell_bazel_calls,
        mcp_bazel_run_calls: parsed.mcp_bazel_run_calls,
        used_expected_bazel_path,
    })
}

fn parse_codex_events(input: &str) -> anyhow::Result<ParsedEvents> {
    let mut parsed = ParsedEvents {
        usage: ProviderUsage::default(),
        model_events: 0,
        agent_message_events: 0,
        tool_calls: 0,
        file_change_events: 0,
        command_output_bytes: 0,
        mcp_output_bytes: 0,
        mcp_bazel_run_calls: 0,
    };
    for (index, line) in input.lines().enumerate() {
        let event: serde_json::Value = serde_json::from_str(line)
            .with_context(|| format!("parse Codex agentic JSONL event {}", index + 1))?;
        let event_type = event.get("type").and_then(serde_json::Value::as_str);
        if event_type == Some("turn.completed") {
            parsed.model_events = parsed.model_events.saturating_add(1);
            parsed.usage.input_tokens = parsed.usage.input_tokens.saturating_add(
                event
                    .pointer("/usage/input_tokens")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0),
            );
            parsed.usage.cached_input_tokens = parsed.usage.cached_input_tokens.saturating_add(
                event
                    .pointer("/usage/cached_input_tokens")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0),
            );
            parsed.usage.output_tokens = parsed.usage.output_tokens.saturating_add(
                event
                    .pointer("/usage/output_tokens")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0),
            );
            parsed.usage.reasoning_output_tokens =
                parsed.usage.reasoning_output_tokens.saturating_add(
                    event
                        .pointer("/usage/reasoning_output_tokens")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0),
                );
        }
        if matches!(event_type, Some("item.started" | "item.completed")) {
            let item_type = event
                .pointer("/item/type")
                .and_then(serde_json::Value::as_str);
            if event_type == Some("item.started")
                && matches!(item_type, Some("command_execution" | "mcp_tool_call"))
            {
                parsed.tool_calls = parsed.tool_calls.saturating_add(1);
            }
            if event_type == Some("item.completed") && item_type == Some("file_change") {
                parsed.file_change_events = parsed.file_change_events.saturating_add(1);
            }
            if event_type == Some("item.completed") && item_type == Some("agent_message") {
                parsed.agent_message_events = parsed.agent_message_events.saturating_add(1);
            }
            if event_type == Some("item.completed") && item_type == Some("command_execution") {
                parsed.command_output_bytes = parsed.command_output_bytes.saturating_add(
                    event
                        .pointer("/item/aggregated_output")
                        .and_then(serde_json::Value::as_str)
                        .map_or(0, |value| value.len() as u64),
                );
            }
            if event_type == Some("item.completed") && item_type == Some("mcp_tool_call") {
                parsed.mcp_output_bytes = parsed.mcp_output_bytes.saturating_add(
                    event
                        .pointer("/item/result/content/0/text")
                        .and_then(serde_json::Value::as_str)
                        .map_or(0, |value| value.len() as u64),
                );
            }
            if event_type == Some("item.started") && item_type == Some("mcp_tool_call") {
                let server = event
                    .pointer("/item/server")
                    .and_then(serde_json::Value::as_str);
                let tool = event
                    .pointer("/item/tool")
                    .and_then(serde_json::Value::as_str);
                if server == Some("bazel") && matches!(tool, Some("bazel.run" | "bazel_run")) {
                    parsed.mcp_bazel_run_calls = parsed.mcp_bazel_run_calls.saturating_add(1);
                }
            }
        }
    }
    Ok(parsed)
}

async fn create_snapshot(
    config: &AgenticConfig,
    task: &AgenticTask,
    corpus: &Path,
    worktree: &Path,
    repository_cache: &Path,
) -> anyhow::Result<Snapshot> {
    if worktree.exists() {
        tokio::fs::remove_dir_all(worktree).await?;
    }
    if let Some(parent) = worktree.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut checkout = Command::new("git");
    checkout
        .args(["-C"])
        .arg(corpus)
        .args(["worktree", "add", "--detach", "--force", "--quiet"])
        .arg(worktree)
        .arg(&config.project.commit);
    run_checked(&mut checkout, "create pinned agentic worktree").await?;

    let source = config.project.resource_path(&task.workspace_overlay);
    let destination = worktree.to_owned();
    tokio::task::spawn_blocking(move || copy_directory(&source, &destination, false)).await??;
    tokio::fs::write(
        worktree.join(".bazelversion"),
        format!("{}\n", config.project.bazel_version),
    )
    .await?;
    append_repository_cache(worktree, repository_cache)?;

    let mut add = git(worktree);
    add.args(["add", "--all"]);
    run_checked(&mut add, "stage agentic snapshot").await?;
    let tree = git_output(worktree, &["write-tree"]).await?;
    let output = git(worktree)
        .env("GIT_AUTHOR_NAME", "Bazel MCP Benchmark")
        .env("GIT_AUTHOR_EMAIL", "benchmark@invalid")
        .env("GIT_COMMITTER_NAME", "Bazel MCP Benchmark")
        .env("GIT_COMMITTER_EMAIL", "benchmark@invalid")
        .args(["commit-tree", &tree, "-m", "benchmark snapshot"])
        .output()
        .await?;
    ensure!(
        output.status.success(),
        "could not commit clean agentic snapshot"
    );
    let base_commit = String::from_utf8(output.stdout)?.trim().to_owned();
    let mut update_head = git(worktree);
    update_head.args(["update-ref", "--no-deref", "HEAD", &base_commit]);
    run_checked(&mut update_head, "activate clean agentic snapshot").await?;
    ensure!(
        git_output(worktree, &["status", "--porcelain"])
            .await?
            .is_empty(),
        "agentic snapshot is not clean"
    );
    let mut protected_hashes: BTreeMap<_, _> = task
        .protected_paths
        .iter()
        .map(|path| {
            let key = path.to_string_lossy().into_owned();
            Ok((key, sha256_file(&worktree.join(path))?))
        })
        .collect::<anyhow::Result<_>>()?;
    protected_hashes.insert(
        ".bazelrc".to_owned(),
        sha256_file(&worktree.join(".bazelrc"))?,
    );
    protected_hashes.insert(
        ".bazelversion".to_owned(),
        sha256_file(&worktree.join(".bazelversion"))?,
    );
    Ok(Snapshot {
        worktree: worktree.to_owned(),
        base_commit,
        protected_hashes,
    })
}

fn append_repository_cache(worktree: &Path, repository_cache: &Path) -> anyhow::Result<()> {
    let path = worktree.join(".bazelrc");
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open benchmark bazelrc {}", path.display()))?;
    writeln!(
        file,
        "\n# Added to the disposable agentic benchmark snapshot.\ncommon --repository_cache={}",
        bazelrc_quote(repository_cache)
    )?;
    Ok(())
}

fn protected_path_violations(snapshot: &Snapshot) -> anyhow::Result<Vec<String>> {
    let mut violations = Vec::new();
    for (path, expected) in &snapshot.protected_hashes {
        let current = sha256_file(&snapshot.worktree.join(path)).ok();
        if current.as_deref() != Some(expected) {
            violations.push(path.clone());
        }
    }
    Ok(violations)
}

async fn capture_patch(
    worktree: &Path,
    base_commit: &str,
) -> anyhow::Result<(Vec<u8>, Vec<String>)> {
    let untracked = git_output_bytes(
        worktree,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )
    .await?;
    for path in untracked
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let mut add = git(worktree);
        add.args(["add", "--intent-to-add", "--"])
            .arg(os_string_from_bytes(path));
        run_checked(&mut add, "record untracked agentic file").await?;
    }
    let patch = git_output_bytes(worktree, &["diff", "--binary", base_commit, "--"]).await?;
    ensure!(
        patch.len() <= MAX_PATCH_BYTES,
        "agentic patch exceeded {} bytes",
        MAX_PATCH_BYTES
    );
    let names =
        git_output_bytes(worktree, &["diff", "--name-only", "-z", base_commit, "--"]).await?;
    let changed_paths = names
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .collect();
    Ok((patch, changed_paths))
}

#[allow(clippy::too_many_arguments)]
async fn run_verifier(
    config: &AgenticConfig,
    task: &AgenticTask,
    worktree: &Path,
    output_root: &Path,
    _repository_cache: &Path,
    verifier_root: &Path,
    bazel: &Path,
) -> anyhow::Result<(Option<i32>, u64)> {
    tokio::fs::create_dir_all(verifier_root).await?;
    let stdout = private_file(&output_root.join("verifier-stdout.log"))?;
    let stderr = private_file(&output_root.join("verifier-stderr.log"))?;
    let started = Instant::now();
    let status = Command::new(bazel)
        .current_dir(worktree)
        .env("USE_BAZEL_VERSION", &config.project.bazel_version)
        .arg(format!("--output_user_root={}", verifier_root.display()))
        .arg(&task.verify_command)
        .args(&task.verify_args)
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .status()
        .await?;
    let verifier_ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
    shutdown_bazel(config, worktree, verifier_root, bazel).await;
    Ok((status.code(), verifier_ms))
}

async fn shutdown_bazel(config: &AgenticConfig, worktree: &Path, output_root: &Path, bazel: &Path) {
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

async fn remove_worktree(corpus: &Path, worktree: &Path) -> anyhow::Result<()> {
    let mut command = Command::new("git");
    command
        .args(["-C"])
        .arg(corpus)
        .args(["worktree", "remove", "--force"])
        .arg(worktree);
    run_checked(&mut command, "remove agentic worktree").await
}

fn summarize(samples: &[AgenticSample], adapters: &[AgenticAdapter]) -> Vec<AgenticSummary> {
    adapters
        .iter()
        .map(|adapter| {
            let matching: Vec<_> = samples
                .iter()
                .filter(|sample| sample.adapter == adapter.name())
                .collect();
            let verified_solves = matching.iter().filter(|sample| sample.verified).count();
            let total_tokens: u64 = matching
                .iter()
                .map(|sample| sample.usage.total_tokens())
                .sum();
            AgenticSummary {
                adapter: adapter.name().to_owned(),
                attempts: matching.len(),
                verified_solves,
                solve_rate_percent: percent(verified_solves, matching.len()),
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
                total_tokens,
                tokens_per_verified_solve: (verified_solves > 0)
                    .then_some(total_tokens as f64 / verified_solves as f64),
                agent_message_events: matching
                    .iter()
                    .map(|sample| sample.agent_message_events)
                    .sum(),
                tool_calls: matching.iter().map(|sample| sample.tool_calls).sum(),
                command_output_bytes: matching
                    .iter()
                    .map(|sample| sample.command_output_bytes)
                    .sum(),
                mcp_output_bytes: matching.iter().map(|sample| sample.mcp_output_bytes).sum(),
                end_to_end_ms: matching.iter().map(|sample| sample.end_to_end_ms).sum(),
            }
        })
        .collect()
}

fn summarize_weighted(
    samples: &[AgenticSample],
    adapters: &[AgenticAdapter],
) -> Vec<AgenticWeightedSummary> {
    adapters
        .iter()
        .flat_map(|adapter| {
            let matching: Vec<_> = samples
                .iter()
                .filter(|sample| sample.adapter == adapter.name())
                .collect();
            let verified_solves = matching.iter().filter(|sample| sample.verified).count();
            CACHE_WEIGHT_PERCENTAGES.map(move |weight| {
                let weighted_units: u64 = matching
                    .iter()
                    .map(|sample| weighted_usage_units(sample, weight))
                    .sum();
                let weighted_tokens = weighted_units as f64 / 100.0;
                AgenticWeightedSummary {
                    adapter: adapter.name().to_owned(),
                    cached_input_weight_percent: weight,
                    weighted_tokens,
                    weighted_tokens_per_verified_solve: (verified_solves > 0)
                        .then_some(weighted_tokens / verified_solves as f64),
                }
            })
        })
        .collect()
}

fn compare_weighted(
    samples: &[AgenticSample],
    adapters: &[AgenticAdapter],
) -> anyhow::Result<Vec<AgenticWeightedComparison>> {
    if !adapters.contains(&AgenticAdapter::BazelMcp) {
        return Ok(Vec::new());
    }
    adapters
        .iter()
        .copied()
        .filter(|adapter| *adapter != AgenticAdapter::BazelMcp)
        .flat_map(|baseline| CACHE_WEIGHT_PERCENTAGES.map(move |weight| (baseline, weight)))
        .map(|(baseline, weight)| {
            let pairs = paired_samples(samples, baseline)?;
            Ok(AgenticWeightedComparison {
                baseline_adapter: baseline.name().to_owned(),
                candidate_adapter: AgenticAdapter::BazelMcp.name().to_owned(),
                cached_input_weight_percent: weight,
                weighted_token_reduction_percent: cluster_bootstrap_reduction(
                    &pairs,
                    |sample| weighted_usage_units(sample, weight),
                    stable_seed(&format!("{}:weighted:{weight}", baseline.name())),
                ),
            })
        })
        .collect()
}

fn weighted_usage_units(sample: &AgenticSample, cached_input_weight_percent: u32) -> u64 {
    let non_cached = sample
        .usage
        .uncached_input_tokens()
        .saturating_add(sample.usage.output_tokens);
    non_cached.saturating_mul(100).saturating_add(
        sample
            .usage
            .cached_input_tokens
            .saturating_mul(u64::from(cached_input_weight_percent)),
    )
}

fn compare(
    samples: &[AgenticSample],
    summaries: &[AgenticSummary],
    adapters: &[AgenticAdapter],
) -> anyhow::Result<Vec<AgenticComparison>> {
    if !adapters.contains(&AgenticAdapter::BazelMcp) {
        return Ok(Vec::new());
    }
    let candidate_summary = summaries
        .iter()
        .find(|summary| summary.adapter == AgenticAdapter::BazelMcp.name())
        .context("agentic MCP summary is missing")?;
    adapters
        .iter()
        .copied()
        .filter(|adapter| *adapter != AgenticAdapter::BazelMcp)
        .map(|baseline| {
            let pairs = paired_samples(samples, baseline)?;
            let concordant: Vec<_> = pairs
                .iter()
                .copied()
                .filter(|(baseline, candidate)| baseline.verified && candidate.verified)
                .collect();
            let baseline_summary = summaries
                .iter()
                .find(|summary| summary.adapter == baseline.name())
                .context("agentic shell summary is missing")?;
            let token_per_solve_reduction = match (
                baseline_summary.tokens_per_verified_solve,
                candidate_summary.tokens_per_verified_solve,
            ) {
                (Some(baseline), Some(candidate)) if baseline > 0.0 => {
                    Some(100.0 * (1.0 - candidate / baseline))
                }
                _ => None,
            };
            let prefix = baseline.name();
            Ok(AgenticComparison {
                baseline_adapter: baseline.name().to_owned(),
                candidate_adapter: AgenticAdapter::BazelMcp.name().to_owned(),
                paired_attempts: pairs.len(),
                concordant_verified_solves: concordant.len(),
                solve_rate_delta_percentage_points: candidate_summary.solve_rate_percent
                    - baseline_summary.solve_rate_percent,
                total_token_reduction_percent: cluster_bootstrap_reduction(
                    &pairs,
                    |sample| sample.usage.total_tokens(),
                    stable_seed(&format!("{prefix}:total")),
                ),
                uncached_input_reduction_percent: cluster_bootstrap_reduction(
                    &pairs,
                    |sample| sample.usage.uncached_input_tokens(),
                    stable_seed(&format!("{prefix}:uncached")),
                ),
                concordant_total_token_reduction_percent: (!concordant.is_empty()).then(|| {
                    cluster_bootstrap_reduction(
                        &concordant,
                        |sample| sample.usage.total_tokens(),
                        stable_seed(&format!("{prefix}:concordant")),
                    )
                }),
                tokens_per_verified_solve_reduction_percent: token_per_solve_reduction,
            })
        })
        .collect()
}

fn compare_tasks(
    samples: &[AgenticSample],
    adapters: &[AgenticAdapter],
) -> anyhow::Result<Vec<AgenticTaskComparison>> {
    if !adapters.contains(&AgenticAdapter::BazelMcp) {
        return Ok(Vec::new());
    }
    let tasks: BTreeSet<_> = samples.iter().map(|sample| sample.task.as_str()).collect();
    let mut comparisons = Vec::new();
    for baseline in adapters
        .iter()
        .copied()
        .filter(|adapter| *adapter != AgenticAdapter::BazelMcp)
    {
        for task in &tasks {
            let baseline_samples: Vec<_> = samples
                .iter()
                .filter(|sample| sample.adapter == baseline.name() && sample.task == *task)
                .collect();
            let candidate_samples: Vec<_> = samples
                .iter()
                .filter(|sample| {
                    sample.adapter == AgenticAdapter::BazelMcp.name() && sample.task == *task
                })
                .collect();
            ensure!(
                !baseline_samples.is_empty() && baseline_samples.len() == candidate_samples.len(),
                "incomplete task-level agentic sample pairs for {task}"
            );
            let sum = |values: &[&AgenticSample], metric: fn(&AgenticSample) -> u64| {
                values.iter().map(|sample| metric(sample)).sum()
            };
            comparisons.push(AgenticTaskComparison {
                task: (*task).to_owned(),
                baseline_adapter: baseline.name().to_owned(),
                candidate_adapter: AgenticAdapter::BazelMcp.name().to_owned(),
                paired_attempts: baseline_samples.len(),
                baseline_verified_solves: baseline_samples
                    .iter()
                    .filter(|sample| sample.verified)
                    .count(),
                candidate_verified_solves: candidate_samples
                    .iter()
                    .filter(|sample| sample.verified)
                    .count(),
                total_token_reduction_percent: reduction(
                    sum(&candidate_samples, sample_total_tokens),
                    sum(&baseline_samples, sample_total_tokens),
                ),
                active_token_reduction_percent: reduction(
                    sum(&candidate_samples, sample_active_tokens),
                    sum(&baseline_samples, sample_active_tokens),
                ),
                tool_output_byte_reduction_percent: reduction(
                    sum(&candidate_samples, sample_tool_output_bytes),
                    sum(&baseline_samples, sample_tool_output_bytes),
                ),
                end_to_end_reduction_percent: reduction(
                    sum(&candidate_samples, sample_end_to_end_ms),
                    sum(&baseline_samples, sample_end_to_end_ms),
                ),
            });
        }
    }
    Ok(comparisons)
}

fn sample_total_tokens(sample: &AgenticSample) -> u64 {
    sample.usage.total_tokens()
}

fn sample_active_tokens(sample: &AgenticSample) -> u64 {
    sample
        .usage
        .uncached_input_tokens()
        .saturating_add(sample.usage.output_tokens)
}

fn sample_tool_output_bytes(sample: &AgenticSample) -> u64 {
    sample
        .command_output_bytes
        .saturating_add(sample.mcp_output_bytes)
}

fn sample_end_to_end_ms(sample: &AgenticSample) -> u64 {
    sample.end_to_end_ms
}

fn paired_samples(
    samples: &[AgenticSample],
    baseline: AgenticAdapter,
) -> anyhow::Result<Vec<(&AgenticSample, &AgenticSample)>> {
    let candidate: BTreeMap<_, _> = samples
        .iter()
        .filter(|sample| sample.adapter == AgenticAdapter::BazelMcp.name())
        .map(|sample| ((sample.task.as_str(), sample.sample), sample))
        .collect();
    let baseline_samples: Vec<_> = samples
        .iter()
        .filter(|sample| sample.adapter == baseline.name())
        .collect();
    ensure!(
        !baseline_samples.is_empty(),
        "agentic comparison has no samples"
    );
    ensure!(
        baseline_samples.len() == candidate.len(),
        "incomplete agentic sample pairs"
    );
    baseline_samples
        .into_iter()
        .map(|sample| {
            let candidate = candidate
                .get(&(sample.task.as_str(), sample.sample))
                .copied()
                .with_context(|| {
                    format!("missing MCP pair for {}/{}", sample.task, sample.sample)
                })?;
            Ok((sample, candidate))
        })
        .collect()
}

fn cluster_bootstrap_reduction(
    pairs: &[(&AgenticSample, &AgenticSample)],
    metric: impl Fn(&AgenticSample) -> u64 + Copy,
    seed: u64,
) -> Estimate {
    let value = reduction(
        pairs.iter().map(|(_, candidate)| metric(candidate)).sum(),
        pairs.iter().map(|(baseline, _)| metric(baseline)).sum(),
    );
    let mut by_task = BTreeMap::<&str, Vec<_>>::new();
    for pair in pairs {
        by_task.entry(&pair.0.task).or_default().push(*pair);
    }
    let clusters: Vec<_> = by_task.into_values().collect();
    let mut random = DeterministicRandom::new(seed);
    let mut estimates = Vec::with_capacity(BOOTSTRAP_RESAMPLES);
    for _ in 0..BOOTSTRAP_RESAMPLES {
        let mut baseline_total = 0_u64;
        let mut candidate_total = 0_u64;
        for _ in 0..clusters.len() {
            for (baseline, candidate) in &clusters[random.index(clusters.len())] {
                baseline_total = baseline_total.saturating_add(metric(baseline));
                candidate_total = candidate_total.saturating_add(metric(candidate));
            }
        }
        estimates.push(reduction(candidate_total, baseline_total));
    }
    estimates.sort_by(f64::total_cmp);
    Estimate {
        value,
        ci95_lower: percentile_sorted(&estimates, 25, 1_000),
        ci95_upper: percentile_sorted(&estimates, 975, 1_000),
    }
}

fn agent_prompt(
    config: &AgenticConfig,
    task: &AgenticTask,
    adapter: AgenticAdapter,
) -> anyhow::Result<String> {
    let policy = match adapter {
        AgenticAdapter::ShellDefault => {
            "Use the shell for Bazel commands. Do not use any MCP tool."
        }
        AgenticAdapter::ShellOptimized => {
            "Use the shell for Bazel commands and no MCP tool. Disable Bazel color and curses, set a 60-second progress rate limit, use --test_output=errors for tests, wait 30 seconds before the first poll, and poll no more often than every 60 seconds."
        }
        AgenticAdapter::ShellMcpLoaded => {
            "Use the shell for every Bazel command. The bazel MCP server is configured only to measure its fixed context overhead; do not call any MCP tool."
        }
        AgenticAdapter::BazelMcp => {
            "Use the shell for repository inspection, Git, and file edits. Use only the bazel MCP server for every Bazel build, test, coverage, or query invocation. Never invoke bazel, bazelisk, tools/bazel, or a command that transitively launches Bazel through the shell. The server owns Bazel presentation flags such as --color, --curses, --show_progress, --show_result, --test_output, and --test_summary; do not pass those flags to MCP. You may call bazel.run as often as the coding task requires and use bazel.inspect only when a bounded result omits evidence needed for the fix."
        }
    };
    Ok(format!(
        "This is a controlled coding benchmark in a disposable Abseil snapshot. Implement the requested fix in the repository; do not merely describe it. Inspect and edit files as needed, run relevant Bazel validation, and do not commit changes. {policy} For Abseil build and test commands, use --features=-module_maps and --macos_minimum_os=14.0.\n\nTask:\n{}\n\nReturn only the requested schema with a concise summary and the validation commands or Bazel targets you ran.",
        config.project.prompt(task)?.trim()
    ))
}

fn verdict_schema() -> &'static [u8] {
    br#"{"type":"object","properties":{"summary":{"type":"string"},"validation":{"type":"array","items":{"type":"string"}}},"required":["summary","validation"],"additionalProperties":false}"#
}

fn validate_relative_path(path: &Path) -> anyhow::Result<()> {
    ensure!(
        !path.as_os_str().is_empty(),
        "resource path must not be empty"
    );
    ensure!(
        path.components()
            .all(|component| matches!(component, Component::Normal(_))),
        "resource path must be relative and traversal-free: {}",
        path.display()
    );
    Ok(())
}

fn os_string_from_bytes(bytes: &[u8]) -> OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(bytes.to_vec())
    }
    #[cfg(not(unix))]
    {
        OsString::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

fn copy_directory(source: &Path, destination: &Path, replace: bool) -> anyhow::Result<()> {
    ensure!(source.is_dir(), "overlay source is not a directory");
    std::fs::create_dir_all(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        ensure!(
            !file_type.is_symlink(),
            "agentic overlays must not contain symlinks"
        );
        let target = destination.join(entry.file_name());
        if file_type.is_dir() {
            copy_directory(&entry.path(), &target, replace)?;
        } else {
            ensure!(
                file_type.is_file(),
                "agentic overlay contains a special file"
            );
            ensure!(
                replace || !target.exists(),
                "agentic overlay target already exists"
            );
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

async fn install_proxy(proxy: &Path, destination: &Path) -> anyhow::Result<()> {
    tokio::fs::create_dir_all(destination).await?;
    tokio::fs::copy(proxy, destination.join("bazel")).await?;
    tokio::fs::copy(proxy, destination.join("bazelisk")).await?;
    Ok(())
}

async fn snapshot_executable(source: &Path, destination: &Path) -> anyhow::Result<PathBuf> {
    if let Some(parent) = destination.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::copy(source, destination)
        .await
        .with_context(|| format!("snapshot benchmark executable {}", source.display()))?;
    ensure!(
        destination.is_file(),
        "benchmark executable snapshot is missing"
    );
    Ok(destination.to_owned())
}

fn benchmark_path(wrapper_dir: &Path) -> anyhow::Result<std::ffi::OsString> {
    let mut paths = vec![wrapper_dir.to_owned()];
    if let Some(path) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&path));
    }
    std::env::join_paths(paths).context("construct agentic benchmark PATH")
}

fn count_wrapper_calls(path: &Path) -> anyhow::Result<u64> {
    match std::fs::read_to_string(path) {
        Ok(input) => {
            for (index, line) in input.lines().enumerate() {
                let _: serde_json::Value = serde_json::from_str(line)
                    .with_context(|| format!("parse shell Bazel call {}", index + 1))?;
            }
            Ok(input.lines().count() as u64)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error.into()),
    }
}

fn sha256_file(path: &Path) -> anyhow::Result<String> {
    let bytes =
        std::fs::read(path).with_context(|| format!("read protected file {}", path.display()))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let mut file = private_file(path)?;
    file.write_all(bytes)?;
    Ok(())
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
        .with_context(|| format!("create private agentic capture {}", path.display()))
}

fn git(worktree: &Path) -> Command {
    let mut command = Command::new("git");
    command.args(["-C"]).arg(worktree);
    command
}

async fn run_checked(command: &mut Command, description: &str) -> anyhow::Result<()> {
    let status = command.status().await?;
    ensure!(status.success(), "could not {description}");
    Ok(())
}

async fn git_output(worktree: &Path, args: &[&str]) -> anyhow::Result<String> {
    let bytes = git_output_bytes(worktree, args).await?;
    Ok(String::from_utf8(bytes)?.trim().to_owned())
}

async fn git_output_bytes(worktree: &Path, args: &[&str]) -> anyhow::Result<Vec<u8>> {
    let output = git(worktree).args(args).output().await?;
    ensure!(output.status.success(), "git command failed");
    Ok(output.stdout)
}

fn server_executable(repository_root: &Path) -> anyhow::Result<PathBuf> {
    let path = std::env::var_os("BAZEL_MCP_SERVER_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| repository_root.join("target/debug/bazel-mcp"));
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

fn toml_path(path: &Path) -> String {
    serde_json::to_string(&path.to_string_lossy()).expect("paths serialize as JSON strings")
}

fn toml_text(value: &str) -> String {
    serde_json::to_string(value).expect("text serializes as a JSON/TOML string")
}

fn bazelrc_quote(path: &Path) -> String {
    let value = path
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    format!("\"{value}\"")
}

fn canonicalize_local_paths(input: &str, worktree: &Path, output_root: &Path) -> String {
    input
        .replace(&worktree.to_string_lossy().to_string(), "<WORKTREE>")
        .replace(&output_root.to_string_lossy().to_string(), "<OUTPUT_ROOT>")
}

fn redact_report_text(input: &str) -> String {
    let patterns = [
        r"(?i)authorization:\s*bearer\s+[^\s]+",
        r"(?i)(api[_-]?key|token|secret|password)\s*[:=]\s*[^\s,;}]+",
        r"sk-[A-Za-z0-9_-]{16,}",
    ]
    .map(str::to_owned);
    Redactor::new(&patterns)
        .expect("built-in benchmark redaction patterns are valid")
        .redact(input)
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

fn percent(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        100.0 * numerator as f64 / denominator as f64
    }
}

fn reduction(candidate: u64, baseline: u64) -> f64 {
    if baseline == 0 {
        0.0
    } else {
        100.0 * (1.0 - candidate as f64 / baseline as f64)
    }
}

fn percentile_sorted(values: &[f64], numerator: usize, denominator: usize) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let index = (values.len() * numerator)
        .div_ceil(denominator)
        .saturating_sub(1)
        .min(values.len() - 1);
    values[index]
}

fn format_estimate(estimate: &Estimate) -> String {
    format!(
        "{:.2}% ({:.2}–{:.2}%)",
        estimate.value, estimate.ci95_lower, estimate.ci95_upper
    )
}

fn optional_number(value: Option<f64>) -> String {
    value.map_or_else(|| "n/a".to_owned(), |value| format!("{value:.0}"))
}

fn optional_percent(value: Option<f64>) -> String {
    value.map_or_else(|| "n/a".to_owned(), |value| format!("{value:.2}%"))
}

struct DeterministicRandom(u64);

impl DeterministicRandom {
    fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    fn index(&mut self, length: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 32) as usize) % length
    }
}

fn stable_seed(value: &str) -> u64 {
    value.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3)
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_checked_in_agentic_manifest() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources/agentic/abseil-cpp.toml");
        let manifest = AgenticProjectManifest::load(&path).unwrap();
        assert_eq!(manifest.tasks.len(), 4);
        assert_eq!(manifest.tasks[0].name, "fix-greeting");
        assert_eq!(manifest.tasks[1].name, "fix-noisy-normalizer");
        assert_eq!(manifest.tasks[2].name, "fix-fanout-macro");
    }

    #[test]
    fn parses_provider_usage_and_mcp_calls() {
        let events = r#"{"type":"item.started","item":{"type":"mcp_tool_call","server":"bazel","tool":"bazel.run"}}
{"type":"item.completed","item":{"type":"mcp_tool_call","result":{"content":[{"type":"text","text":"bounded"}]}}}
{"type":"item.completed","item":{"type":"command_execution","aggregated_output":"raw output"}}
{"type":"item.completed","item":{"type":"agent_message","text":"done"}}
{"type":"item.completed","item":{"type":"file_change"}}
{"type":"turn.completed","usage":{"input_tokens":100,"cached_input_tokens":60,"output_tokens":20,"reasoning_output_tokens":3}}
"#;
        let parsed = parse_codex_events(events).unwrap();
        assert_eq!(parsed.usage.total_tokens(), 120);
        assert_eq!(parsed.usage.uncached_input_tokens(), 40);
        assert_eq!(parsed.mcp_bazel_run_calls, 1);
        assert_eq!(parsed.file_change_events, 1);
        assert_eq!(parsed.agent_message_events, 1);
        assert_eq!(parsed.command_output_bytes, 10);
        assert_eq!(parsed.mcp_output_bytes, 7);
    }

    #[test]
    fn redacts_secrets_from_report_fields() {
        let redacted = redact_report_text("token=super-secret authorization: Bearer abc123");
        assert!(!redacted.contains("super-secret"));
        assert!(!redacted.contains("abc123"));
    }

    #[test]
    fn comparison_gates_concordant_solve_metrics() {
        let samples = vec![
            sample("shell-default", "one", 100, true),
            sample("bazel-mcp", "one", 40, false),
        ];
        let adapters = vec![AgenticAdapter::ShellDefault, AgenticAdapter::BazelMcp];
        let summaries = summarize(&samples, &adapters);
        let comparisons = compare(&samples, &summaries, &adapters).unwrap();
        assert_eq!(comparisons[0].total_token_reduction_percent.value, 60.0);
        assert_eq!(comparisons[0].solve_rate_delta_percentage_points, -100.0);
        assert!(
            comparisons[0]
                .concordant_total_token_reduction_percent
                .is_none()
        );
    }

    #[test]
    fn clustered_bootstrap_spans_different_task_effects() {
        let samples = vec![
            sample("shell-default", "one", 100, true),
            sample("bazel-mcp", "one", 50, true),
            sample("shell-default", "two", 100, true),
            sample("bazel-mcp", "two", 150, true),
        ];
        let adapters = vec![AgenticAdapter::ShellDefault, AgenticAdapter::BazelMcp];
        let summaries = summarize(&samples, &adapters);
        let comparisons = compare(&samples, &summaries, &adapters).unwrap();
        let estimate = &comparisons[0].total_token_reduction_percent;
        assert_eq!(estimate.value, 0.0);
        assert!(estimate.ci95_lower <= -50.0);
        assert!(estimate.ci95_upper >= 50.0);
    }

    #[test]
    fn weighted_usage_varies_only_cached_input() {
        let mut value = sample("shell-default", "one", 100, true);
        value.usage.cached_input_tokens = 60;
        value.usage.output_tokens = 20;
        assert_eq!(weighted_usage_units(&value, 0), 6_000);
        assert_eq!(weighted_usage_units(&value, 25), 7_500);
        assert_eq!(weighted_usage_units(&value, 100), 12_000);
    }

    #[test]
    fn task_comparisons_keep_task_effects_visible() {
        let mut baseline = sample("shell-default", "noisy", 200, true);
        baseline.command_output_bytes = 1_000;
        baseline.end_to_end_ms = 100;
        let mut candidate = sample("bazel-mcp", "noisy", 100, true);
        candidate.mcp_output_bytes = 100;
        candidate.end_to_end_ms = 120;
        let comparisons = compare_tasks(
            &[baseline, candidate],
            &[AgenticAdapter::ShellDefault, AgenticAdapter::BazelMcp],
        )
        .unwrap();
        assert_eq!(comparisons.len(), 1);
        assert_eq!(comparisons[0].total_token_reduction_percent, 50.0);
        assert_eq!(comparisons[0].tool_output_byte_reduction_percent, 90.0);
        assert!((comparisons[0].end_to_end_reduction_percent + 20.0).abs() < 1e-12);
    }

    #[test]
    fn shell_mcp_loaded_is_a_shell_control_with_mcp_context() {
        assert!(AgenticAdapter::ShellMcpLoaded.loads_mcp());
        assert!(AgenticAdapter::ShellMcpLoaded.uses_shell_bazel());
        assert_eq!(AgenticAdapter::ShellMcpLoaded.name(), "shell-mcp-loaded");
    }

    #[tokio::test]
    async fn snapshot_is_a_clean_root_and_patch_captures_new_files() {
        let temporary = tempfile::tempdir().unwrap();
        let repository_root = temporary.path();
        let corpus = repository_root.join("corpus");
        std::fs::create_dir_all(&corpus).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet"])
                .arg(&corpus)
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(corpus.join("source.txt"), "pinned\n").unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["-C"])
                .arg(&corpus)
                .args(["add", "source.txt"])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            std::process::Command::new("git")
                .args(["-C"])
                .arg(&corpus)
                .args([
                    "-c",
                    "user.name=Benchmark Test",
                    "-c",
                    "user.email=test@invalid",
                    "commit",
                    "--quiet",
                    "-m",
                    "pinned",
                ])
                .status()
                .unwrap()
                .success()
        );
        let commit = String::from_utf8(
            std::process::Command::new("git")
                .args(["-C"])
                .arg(&corpus)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_owned();
        let resources = repository_root.join("resources");
        std::fs::create_dir_all(resources.join("workspace/package")).unwrap();
        std::fs::write(resources.join("prompt.md"), "Fix the task.\n").unwrap();
        std::fs::write(resources.join("workspace/package/task.txt"), "broken\n").unwrap();
        let task = AgenticTask {
            name: "task".to_owned(),
            prompt_file: PathBuf::from("prompt.md"),
            workspace_overlay: PathBuf::from("workspace"),
            verification_overlay: None,
            verify_command: "test".to_owned(),
            verify_args: vec!["//package:test".to_owned()],
            protected_paths: vec![PathBuf::from("package/task.txt")],
            timeout_seconds: 60,
        };
        let config = AgenticConfig {
            repository_root: repository_root.to_owned(),
            project: AgenticProjectManifest {
                name: "test".to_owned(),
                url: "local".to_owned(),
                commit,
                license: "test".to_owned(),
                bazel_version: "9.1.0".to_owned(),
                tasks: vec![task.clone()],
                resource_root: resources,
            },
            samples: 1,
            task_filter: BTreeSet::new(),
            adapters: vec![AgenticAdapter::ShellDefault],
            keep_worktrees: false,
            codex_executable: PathBuf::from("/bin/true"),
            proxy_executable: PathBuf::from("/bin/true"),
            model: None,
            reasoning_effort: None,
            timeout_override: None,
        };
        let worktree = repository_root.join("worktree");
        let repository_cache = repository_root.join("repository-cache");
        let snapshot = create_snapshot(&config, &task, &corpus, &worktree, &repository_cache)
            .await
            .unwrap();
        let parents = git_output(&worktree, &["rev-list", "--parents", "-n", "1", "HEAD"])
            .await
            .unwrap();
        assert_eq!(parents.split_whitespace().count(), 1);
        assert!(
            git_output(&worktree, &["status", "--porcelain"])
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            std::fs::read_to_string(worktree.join(".bazelversion")).unwrap(),
            "9.1.0\n"
        );
        std::fs::write(worktree.join(".bazelversion"), "9.2.0\n").unwrap();
        assert_eq!(
            protected_path_violations(&snapshot).unwrap(),
            vec![".bazelversion".to_owned()]
        );
        std::fs::write(worktree.join(".bazelversion"), "9.1.0\n").unwrap();
        std::fs::write(worktree.join("package/task.txt"), "fixed\n").unwrap();
        std::fs::write(worktree.join("package/new.txt"), "new\n").unwrap();
        let (patch, changed) = capture_patch(&worktree, &snapshot.base_commit)
            .await
            .unwrap();
        let patch = String::from_utf8(patch).unwrap();
        assert!(patch.contains("fixed"));
        assert!(patch.contains("new.txt"));
        assert_eq!(
            changed,
            vec!["package/new.txt".to_owned(), "package/task.txt".to_owned()]
        );
        remove_worktree(&corpus, &worktree).await.unwrap();
    }

    fn sample(adapter: &str, task: &str, tokens: u64, verified: bool) -> AgenticSample {
        AgenticSample {
            adapter: adapter.to_owned(),
            task: task.to_owned(),
            sample: 0,
            verified,
            verifier_exit_code: Some(if verified { 0 } else { 1 }),
            protected_paths_unchanged: true,
            protected_path_violations: Vec::new(),
            used_expected_bazel_path: true,
            shell_bazel_calls: u64::from(adapter.starts_with("shell")),
            mcp_bazel_run_calls: u64::from(adapter == "bazel-mcp"),
            model_events: 1,
            agent_message_events: 1,
            tool_calls: 1,
            file_change_events: 1,
            command_output_bytes: 0,
            mcp_output_bytes: 0,
            changed_paths: vec!["file.cc".to_owned()],
            patch_bytes: 1,
            end_to_end_ms: 1,
            verifier_ms: 1,
            usage: ProviderUsage {
                input_tokens: tokens,
                cached_input_tokens: 0,
                output_tokens: 0,
                reasoning_output_tokens: 0,
            },
            final_summary: String::new(),
            reported_validation: Vec::new(),
        }
    }
}
