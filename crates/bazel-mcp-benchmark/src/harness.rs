use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    process::Stdio,
    time::{Instant, SystemTime},
};

use anyhow::{Context, ensure};
use bazel_mcp_reducer::normalize_terminal_text;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{ChildStdout, Command},
};

use crate::{
    AdapterMetrics, BaselineComparison, BenchmarkReport, EnvironmentMetadata, Estimate,
    ProjectManifest, Scenario, SummaryStatistics,
    transcript::{Transcript, TranscriptEvent, TranscriptKind},
};

const BOOTSTRAP_RESAMPLES: usize = 4_000;

const SYSTEM_PROMPT: &str = "You are a coding agent operating in a large Bazel repository. Run the requested validation, wait for it to finish, and identify the actionable root cause on failure. Tool results and prior messages remain in context at every model decision.";
const OPTIMIZED_INSTRUCTIONS: &str = "Batch compatible targets. Use a 30-second initial yield. If still running, poll once after 30 seconds and then no more often than every 60 seconds. Use concise progress updates. Configure Bazel with color and curses disabled, a 60-second progress rate limit, and test output only on errors. Read a failed-test log at most once.";
const SHELL_SCHEMA: &str = r#"exec_command({cmd:string,workdir:string,yield_time_ms?:integer,max_output_tokens?:integer}); write_stdin({session_id:integer,yield_time_ms?:integer,max_output_tokens?:integer})"#;
const MCP_SCHEMA: &str = r#"bazel.run({workspace:absolute_path,startup_args?:string[],command:string,args?:string[],timeout_seconds?:integer}) -> one bounded invocation summary"#;

#[derive(Clone, Debug)]
pub struct HarnessConfig {
    pub repository_root: PathBuf,
    pub project: ProjectManifest,
    pub encoding: String,
    pub samples: u32,
    pub assert_gates: bool,
    pub scenario_filter: BTreeSet<String>,
    pub cache_conditions: Vec<String>,
    pub keep_worktree: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Adapter {
    ShellDefault,
    ShellOptimized,
    BazelMcp,
}

impl Adapter {
    const ALL: [Self; 3] = [Self::ShellDefault, Self::ShellOptimized, Self::BazelMcp];

    const fn name(self) -> &'static str {
        match self {
            Self::ShellDefault => "shell-default",
            Self::ShellOptimized => "shell-optimized",
            Self::BazelMcp => "bazel-mcp",
        }
    }
}

#[derive(Debug)]
struct Observation {
    exit_code: Option<i32>,
    bazel_wall_ms: u64,
    end_to_end_ms: u64,
    model_results: Vec<String>,
    raw_evidence: String,
}

pub async fn run_integration(config: HarnessConfig) -> anyhow::Result<BenchmarkReport> {
    let run_id = SystemTime::UNIX_EPOCH.elapsed()?.as_millis().to_string();
    let adapter_order_seed = run_id.parse::<u64>()?;
    let run_root = config
        .repository_root
        .join(".cache/benchmarks")
        .join(&config.project.name)
        .join(&run_id);
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

    let scenarios: Vec<_> = config
        .project
        .scenarios
        .iter()
        .filter(|scenario| {
            config.scenario_filter.is_empty() || config.scenario_filter.contains(&scenario.name)
        })
        .cloned()
        .collect();
    ensure!(!scenarios.is_empty(), "no scenarios selected");

    let mut samples = Vec::new();
    for cache_condition in &config.cache_conditions {
        ensure!(
            matches!(cache_condition.as_str(), "cold" | "warm"),
            "cache condition must be cold or warm"
        );
        for sample in 0..config.samples.max(1) {
            for scenario in &scenarios {
                let mut adapters = Adapter::ALL;
                let adapter_count = adapters.len();
                let rotation = (adapter_order_seed as usize
                    + sample as usize
                    + scenario.name.len()
                    + cache_condition.len())
                    % adapter_count;
                adapters.rotate_left(rotation);
                for adapter in adapters {
                    let output_root = run_root
                        .join("output-roots")
                        .join(cache_condition)
                        .join(sample.to_string())
                        .join(sanitize(&scenario.name))
                        .join(adapter.name());
                    if output_root.exists() {
                        tokio::fs::remove_dir_all(&output_root).await?;
                    }
                    tokio::fs::create_dir_all(&output_root).await?;
                    if cache_condition == "warm" {
                        let _ = run_adapter(
                            adapter,
                            &config,
                            &worktree,
                            scenario,
                            &output_root,
                            &run_root,
                        )
                        .await?;
                    }
                    let observation = run_adapter(
                        adapter,
                        &config,
                        &worktree,
                        scenario,
                        &output_root,
                        &run_root,
                    )
                    .await?;
                    shutdown_bazel(&config, &worktree, &output_root).await;
                    let artifact_name = format!(
                        "{}-{}-{}-{}",
                        cache_condition,
                        sample,
                        sanitize(&scenario.name),
                        adapter.name()
                    );
                    tokio::fs::create_dir_all(run_root.join("evidence")).await?;
                    let evidence_path = run_root
                        .join("evidence")
                        .join(format!("{artifact_name}.log"));
                    tokio::fs::write(
                        &evidence_path,
                        canonicalize_output(&observation.raw_evidence, &worktree, &output_root),
                    )
                    .await?;
                    ensure!(
                        observation.exit_code == Some(scenario.expected_exit),
                        "{} {} expected exit {}, observed {:?}; evidence: {}",
                        adapter.name(),
                        scenario.name,
                        scenario.expected_exit,
                        observation.exit_code,
                        evidence_path.display()
                    );
                    let diagnostic_found = scenario.expected_cause.as_ref().is_none_or(|cause| {
                        observation
                            .model_results
                            .iter()
                            .any(|result| result.contains(cause))
                    });
                    let transcript = build_transcript(
                        adapter,
                        scenario,
                        observation.bazel_wall_ms,
                        observation
                            .model_results
                            .iter()
                            .map(|result| canonicalize_output(result, &worktree, &output_root))
                            .collect(),
                    );
                    let transcript_metrics = transcript.measure(&config.encoding)?;
                    tokio::fs::create_dir_all(run_root.join("transcripts")).await?;
                    tokio::fs::write(
                        run_root
                            .join("transcripts")
                            .join(format!("{artifact_name}.jsonl")),
                        transcript.to_jsonl()?,
                    )
                    .await?;
                    let raw_process_bytes = observation.raw_evidence.len() as u64;
                    samples.push(AdapterMetrics {
                        adapter: adapter.name().to_owned(),
                        scenario: scenario.name.clone(),
                        cache_condition: cache_condition.clone(),
                        sample,
                        bazel_wall_ms: observation.bazel_wall_ms,
                        end_to_end_ms: observation.end_to_end_ms,
                        exit_code: observation.exit_code,
                        diagnostic_found,
                        raw_process_bytes,
                        transcript: transcript_metrics,
                    });
                }
            }
        }
    }

    let comparisons = [Adapter::ShellDefault, Adapter::ShellOptimized]
        .into_iter()
        .map(|baseline| build_comparison(&samples, baseline, &config.cache_conditions))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let default_comparison = comparisons
        .iter()
        .find(|comparison| comparison.baseline_adapter == Adapter::ShellDefault.name())
        .context("default shell comparison is missing")?;
    let aggregate_reduction_percent = point_estimates(&default_comparison.aggregate);
    let reduction_percent_by_cache = default_comparison
        .by_cache
        .iter()
        .map(|(cache, estimates)| (cache.clone(), point_estimates(estimates)))
        .collect();
    let statistics = statistics(&samples);
    let report = BenchmarkReport {
        schema_version: 3,
        project: config.project.name.clone(),
        commit: config.project.commit.clone(),
        bazel_version: config.project.bazel_version.clone(),
        tokenizer_crate_version: "0.12.0".to_owned(),
        encoding: config.encoding.clone(),
        canonicalization_version: 1,
        adapter_order_seed,
        environment: environment_metadata(),
        samples,
        statistics,
        comparisons,
        aggregate_reduction_percent,
        reduction_percent_by_cache,
    };
    tokio::fs::write(
        run_root.join("report.json"),
        serde_json::to_vec_pretty(&report)?,
    )
    .await?;
    tokio::fs::write(run_root.join("report.md"), report.markdown()).await?;
    tokio::fs::write(
        config
            .repository_root
            .join(".cache/benchmarks")
            .join(&config.project.name)
            .join("LATEST"),
        format!("{run_id}\n"),
    )
    .await?;

    if !config.keep_worktree {
        remove_worktree(&corpus, &worktree).await?;
    }
    if config.assert_gates {
        ensure!(
            config.samples >= 5,
            "an acceptance run requires at least five measured samples"
        );
        assert_gates(&report, &scenarios)?;
    }
    Ok(report)
}

/// Recompute schema-v3 statistics and paired confidence intervals from a
/// completed schema-v2-or-newer run without re-executing Bazel. Raw samples are
/// retained byte-for-byte; the report and Markdown summary are replaced.
pub async fn recompute_report(path: &Path) -> anyhow::Result<BenchmarkReport> {
    let bytes = tokio::fs::read(path)
        .await
        .with_context(|| format!("read benchmark report {}", path.display()))?;
    let mut report: BenchmarkReport = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse benchmark report {}", path.display()))?;
    ensure!(
        report.schema_version >= 2,
        "only schema-v2-or-newer reports can be recomputed"
    );
    let cache_conditions: Vec<_> = report
        .samples
        .iter()
        .map(|sample| sample.cache_condition.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    ensure!(
        !cache_conditions.is_empty(),
        "report has no benchmark samples"
    );
    report.comparisons = [Adapter::ShellDefault, Adapter::ShellOptimized]
        .into_iter()
        .map(|baseline| build_comparison(&report.samples, baseline, &cache_conditions))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let default_comparison = report
        .comparisons
        .iter()
        .find(|comparison| comparison.baseline_adapter == Adapter::ShellDefault.name())
        .context("default shell comparison is missing")?;
    report.aggregate_reduction_percent = point_estimates(&default_comparison.aggregate);
    report.reduction_percent_by_cache = default_comparison
        .by_cache
        .iter()
        .map(|(cache, estimates)| (cache.clone(), point_estimates(estimates)))
        .collect();
    report.statistics = statistics(&report.samples);
    report.schema_version = 3;
    tokio::fs::write(path, serde_json::to_vec_pretty(&report)?).await?;
    tokio::fs::write(path.with_file_name("report.md"), report.markdown()).await?;
    Ok(report)
}

pub fn assert_acceptance_gates(
    report: &BenchmarkReport,
    scenarios: &[Scenario],
) -> anyhow::Result<()> {
    assert_gates(report, scenarios)
}

async fn shutdown_bazel(config: &HarnessConfig, worktree: &Path, output_root: &Path) {
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

async fn run_adapter(
    adapter: Adapter,
    config: &HarnessConfig,
    worktree: &Path,
    scenario: &Scenario,
    output_root: &Path,
    run_root: &Path,
) -> anyhow::Result<Observation> {
    match adapter {
        Adapter::ShellDefault | Adapter::ShellOptimized => {
            run_shell(adapter, config, worktree, scenario, output_root, run_root).await
        }
        Adapter::BazelMcp => run_mcp(config, worktree, scenario, output_root, run_root).await,
    }
}

async fn run_shell(
    adapter: Adapter,
    config: &HarnessConfig,
    worktree: &Path,
    scenario: &Scenario,
    output_root: &Path,
    run_root: &Path,
) -> anyhow::Result<Observation> {
    let bazel = bazel_executable()?;
    let mut command = Command::new(&bazel);
    command
        .current_dir(worktree)
        .env("USE_BAZEL_VERSION", &config.project.bazel_version)
        .arg(format!("--output_user_root={}", output_root.display()))
        .arg(&scenario.command)
        .arg(format!(
            "--repository_cache={}",
            run_root.join("repository-cache").display()
        ));
    if adapter == Adapter::ShellOptimized {
        command.args(["--color=no", "--curses=no", "--show_progress_rate_limit=60"]);
        if scenario.command == "test" {
            command.arg("--test_output=errors");
        }
    }
    command.args(&scenario.args);
    let started = Instant::now();
    let output = command.output().await.context("run shell Bazel adapter")?;
    let elapsed = started.elapsed().as_millis() as u64;
    let exit_code = output.status.code();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let raw = format!("STDOUT\n{stdout}\nSTDERR\n{stderr}");
    let mut model_results = vec![raw.clone()];
    if scenario.command == "test"
        && !output.status.success()
        && let Some(test_log) = find_test_log(output_root).await?
    {
        model_results.push(format!("failed test log\n{test_log}"));
    }
    let raw_evidence = model_results.join("\n\nFOLLOW-UP\n");
    Ok(Observation {
        exit_code,
        bazel_wall_ms: elapsed,
        end_to_end_ms: elapsed,
        model_results,
        raw_evidence,
    })
}

async fn run_mcp(
    config: &HarnessConfig,
    worktree: &Path,
    scenario: &Scenario,
    output_root: &Path,
    run_root: &Path,
) -> anyhow::Result<Observation> {
    let server_binary = std::env::var_os("BAZEL_MCP_SERVER_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| config.repository_root.join("target/debug/bazel-mcp"));
    ensure!(
        server_binary.is_file(),
        "bazel-mcp binary is missing at {}",
        server_binary.display()
    );
    let server_config = HarnessServerConfig {
        allowed_roots: vec![worktree.to_owned()],
        cache_root: output_root.join("mcp-store"),
        bazel_executable: bazel_executable()?,
        output_user_root: output_root.to_owned(),
        environment_allowlist: BTreeSet::from(["USE_BAZEL_VERSION".to_owned()]),
    };
    let config_path = output_root.join("server.toml");
    tokio::fs::write(&config_path, toml::to_string_pretty(&server_config)?).await?;

    let mut child = Command::new(&server_binary)
        .arg("--config")
        .arg(&config_path)
        .arg("--log")
        .arg("error")
        .env("USE_BAZEL_VERSION", &config.project.bazel_version)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    let mut stdin = child.stdin.take().context("open bazel-mcp stdin")?;
    let stdout = child.stdout.take().context("open bazel-mcp stdout")?;
    let mut lines = BufReader::new(stdout).lines();
    write_rpc(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "bazel-mcp-benchmark", "version": "0.1.0"}
            }
        }),
    )
    .await?;
    let initialize = read_rpc(&mut lines, 1).await?;
    ensure!(
        initialize.get("error").is_none(),
        "MCP initialize failed: {initialize}"
    );
    write_rpc(
        &mut stdin,
        &serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .await?;
    let arguments = serde_json::json!({
        "workspace": worktree,
        "command": scenario.command,
        "args": scenario_args(scenario, run_root),
        "timeout_seconds": 7200,
    });
    let started = Instant::now();
    let result = call_tool(&mut stdin, &mut lines, 2, "bazel.run", arguments).await?;
    let end_to_end_ms = started.elapsed().as_millis() as u64;
    let content = tool_text(&result)?;
    let parsed: McpRunResult = serde_json::from_str(&content)?;
    let mut model_results = vec![content.clone()];
    if scenario
        .expected_cause
        .as_ref()
        .is_some_and(|cause| !content.contains(cause))
    {
        let view = if scenario.command == "test" {
            "tests"
        } else {
            "diagnostics"
        };
        let inspected = call_tool(
            &mut stdin,
            &mut lines,
            3,
            "bazel.inspect",
            serde_json::json!({
                "invocation_id": parsed.invocation_id,
                "view": view,
                "limit": 20
            }),
        )
        .await?;
        model_results.push(tool_text(&inspected)?);
    }
    drop(stdin);
    let status = child.wait().await?;
    ensure!(
        status.success(),
        "bazel-mcp server exited unsuccessfully: {status}"
    );
    Ok(Observation {
        exit_code: parsed.exit_code,
        bazel_wall_ms: parsed.duration_ms,
        end_to_end_ms,
        raw_evidence: model_results.join("\n\nINSPECT\n"),
        model_results,
    })
}

#[derive(Debug, Deserialize)]
struct McpRunResult {
    invocation_id: String,
    exit_code: Option<i32>,
    duration_ms: u64,
}

#[derive(Debug, Serialize)]
struct HarnessServerConfig {
    allowed_roots: Vec<PathBuf>,
    cache_root: PathBuf,
    bazel_executable: PathBuf,
    output_user_root: PathBuf,
    environment_allowlist: BTreeSet<String>,
}

async fn write_rpc(
    stdin: &mut tokio::process::ChildStdin,
    message: &serde_json::Value,
) -> anyhow::Result<()> {
    let mut bytes = serde_json::to_vec(message)?;
    bytes.push(b'\n');
    stdin.write_all(&bytes).await?;
    stdin.flush().await?;
    Ok(())
}

async fn read_rpc(
    lines: &mut Lines<BufReader<ChildStdout>>,
    id: u64,
) -> anyhow::Result<serde_json::Value> {
    while let Some(line) = lines.next_line().await? {
        let message: serde_json::Value = serde_json::from_str(&line)
            .with_context(|| format!("non-protocol data on MCP stdout: {line:?}"))?;
        if message.get("id").and_then(serde_json::Value::as_u64) == Some(id) {
            return Ok(message);
        }
    }
    anyhow::bail!("MCP server closed stdout before response {id}")
}

async fn call_tool(
    stdin: &mut tokio::process::ChildStdin,
    lines: &mut Lines<BufReader<ChildStdout>>,
    id: u64,
    name: &str,
    arguments: serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    write_rpc(
        stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments}
        }),
    )
    .await?;
    let response = read_rpc(lines, id).await?;
    ensure!(
        response.get("error").is_none(),
        "MCP tool request failed: {response}"
    );
    ensure!(
        response
            .pointer("/result/isError")
            .and_then(serde_json::Value::as_bool)
            != Some(true),
        "MCP tool returned isError: {response}"
    );
    Ok(response)
}

fn tool_text(response: &serde_json::Value) -> anyhow::Result<String> {
    response
        .pointer("/result/content/0/text")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .context("MCP tool response has no text content")
}

fn scenario_args(scenario: &Scenario, run_root: &Path) -> Vec<String> {
    let mut arguments = vec![format!(
        "--repository_cache={}",
        run_root.join("repository-cache").display()
    )];
    arguments.extend(scenario.args.iter().cloned());
    arguments
}

fn build_transcript(
    adapter: Adapter,
    scenario: &Scenario,
    elapsed_ms: u64,
    model_results: Vec<String>,
) -> Transcript {
    let mut transcript = Transcript {
        canonicalization_version: 1,
        events: Vec::new(),
    };
    push_event(
        &mut transcript,
        adapter,
        scenario,
        TranscriptKind::System,
        "system",
        SYSTEM_PROMPT,
    );
    if adapter == Adapter::ShellOptimized {
        push_event(
            &mut transcript,
            adapter,
            scenario,
            TranscriptKind::System,
            "system",
            OPTIMIZED_INSTRUCTIONS,
        );
    }
    push_event(
        &mut transcript,
        adapter,
        scenario,
        TranscriptKind::ToolSchema,
        "system",
        if adapter == Adapter::BazelMcp {
            MCP_SCHEMA
        } else {
            SHELL_SCHEMA
        },
    );
    push_event(
        &mut transcript,
        adapter,
        scenario,
        TranscriptKind::Task,
        "user",
        &format!(
            "Run `bazel {} {}` in Abseil and report whether it succeeds{}.",
            scenario.command,
            scenario.args.join(" "),
            scenario
                .expected_cause
                .as_ref()
                .map_or(String::new(), |cause| format!(
                    ", identifying {cause} on failure"
                ))
        ),
    );
    push_event(
        &mut transcript,
        adapter,
        scenario,
        TranscriptKind::ModelEvent,
        "assistant",
        "",
    );
    push_event(
        &mut transcript,
        adapter,
        scenario,
        TranscriptKind::ToolCall,
        "assistant",
        &format!("{} {}", scenario.command, scenario.args.join(" ")),
    );
    for poll_at in poll_schedule(adapter, elapsed_ms) {
        push_event(
            &mut transcript,
            adapter,
            scenario,
            TranscriptKind::Progress,
            "tool",
            &format!("Bazel is still running after {} seconds.", poll_at / 1000),
        );
        push_event(
            &mut transcript,
            adapter,
            scenario,
            TranscriptKind::ModelEvent,
            "assistant",
            "",
        );
        push_event(
            &mut transcript,
            adapter,
            scenario,
            TranscriptKind::ToolCall,
            "assistant",
            "poll process",
        );
    }
    for (index, model_result) in model_results.iter().enumerate() {
        push_event(
            &mut transcript,
            adapter,
            scenario,
            TranscriptKind::ToolResult,
            "tool",
            model_result,
        );
        if index + 1 < model_results.len() {
            push_event(
                &mut transcript,
                adapter,
                scenario,
                TranscriptKind::ModelEvent,
                "assistant",
                "",
            );
            push_event(
                &mut transcript,
                adapter,
                scenario,
                TranscriptKind::ToolCall,
                "assistant",
                "read the failed test log once",
            );
        }
    }
    push_event(
        &mut transcript,
        adapter,
        scenario,
        TranscriptKind::ModelEvent,
        "assistant",
        "",
    );
    transcript
}

fn push_event(
    transcript: &mut Transcript,
    adapter: Adapter,
    scenario: &Scenario,
    kind: TranscriptKind,
    role: &str,
    content: &str,
) {
    transcript.events.push(TranscriptEvent {
        sequence: transcript.events.len() as u64,
        adapter: adapter.name().to_owned(),
        scenario: scenario.name.clone(),
        kind,
        role: role.to_owned(),
        model_visible: true,
        content: content.to_owned(),
    });
}

fn poll_schedule(adapter: Adapter, elapsed_ms: u64) -> Vec<u64> {
    let (mut next, subsequent) = match adapter {
        Adapter::ShellDefault => (10_000, 5_000),
        Adapter::ShellOptimized => (30_000, 60_000),
        Adapter::BazelMcp => return Vec::new(),
    };
    let mut polls = Vec::new();
    while next < elapsed_ms && polls.len() < 240 {
        polls.push(next);
        next = next.saturating_add(if adapter == Adapter::ShellOptimized && polls.len() == 1 {
            30_000
        } else {
            subsequent
        });
    }
    polls
}

fn build_comparison(
    samples: &[AdapterMetrics],
    baseline: Adapter,
    cache_conditions: &[String],
) -> anyhow::Result<BaselineComparison> {
    let aggregate = compare(samples, baseline, None)?;
    let by_cache = cache_conditions
        .iter()
        .map(|cache| Ok((cache.clone(), compare(samples, baseline, Some(cache))?)))
        .collect::<anyhow::Result<_>>()?;
    Ok(BaselineComparison {
        baseline_adapter: baseline.name().to_owned(),
        candidate_adapter: Adapter::BazelMcp.name().to_owned(),
        aggregate,
        by_cache,
    })
}

fn compare(
    samples: &[AdapterMetrics],
    baseline: Adapter,
    cache_condition: Option<&str>,
) -> anyhow::Result<BTreeMap<String, Estimate>> {
    let pairs = paired_samples(samples, baseline, cache_condition)?;
    let seed_prefix = format!("{}:{}", baseline.name(), cache_condition.unwrap_or("all"));
    Ok(BTreeMap::from([
        (
            "cumulative_context_tokens".to_owned(),
            bootstrap_reduction(
                &pairs,
                |sample| sample.transcript.cumulative_context_tokens,
                stable_seed(&format!("{seed_prefix}:context")),
            ),
        ),
        (
            "model_visible_bytes".to_owned(),
            bootstrap_reduction(
                &pairs,
                |sample| sample.transcript.model_visible_bytes,
                stable_seed(&format!("{seed_prefix}:bytes")),
            ),
        ),
        (
            "visible_tool_tokens".to_owned(),
            bootstrap_reduction(
                &pairs,
                |sample| sample.transcript.visible_tool_tokens,
                stable_seed(&format!("{seed_prefix}:tool")),
            ),
        ),
        (
            "bazel_wall_overhead".to_owned(),
            bootstrap_wall_overhead(&pairs, stable_seed(&format!("{seed_prefix}:wall"))),
        ),
    ]))
}

fn paired_samples<'a>(
    samples: &'a [AdapterMetrics],
    baseline: Adapter,
    cache_condition: Option<&str>,
) -> anyhow::Result<Vec<(&'a AdapterMetrics, &'a AdapterMetrics)>> {
    let key = |sample: &'a AdapterMetrics| {
        (
            sample.cache_condition.as_str(),
            sample.scenario.as_str(),
            sample.sample,
        )
    };
    let candidate: BTreeMap<_, _> = samples
        .iter()
        .filter(|sample| {
            sample.adapter == Adapter::BazelMcp.name()
                && cache_condition.is_none_or(|cache| sample.cache_condition == cache)
        })
        .map(|sample| (key(sample), sample))
        .collect();
    let baseline_samples: Vec<_> = samples
        .iter()
        .filter(|sample| {
            sample.adapter == baseline.name()
                && cache_condition.is_none_or(|cache| sample.cache_condition == cache)
        })
        .collect();
    ensure!(!baseline_samples.is_empty(), "comparison has no samples");
    ensure!(
        baseline_samples.len() == candidate.len(),
        "incomplete {} versus {} sample pairs",
        baseline.name(),
        Adapter::BazelMcp.name()
    );
    baseline_samples
        .into_iter()
        .map(|baseline_sample| {
            let candidate_sample =
                candidate
                    .get(&key(baseline_sample))
                    .copied()
                    .with_context(|| {
                        format!(
                            "missing {} sample for {}/{}/{}",
                            Adapter::BazelMcp.name(),
                            baseline_sample.cache_condition,
                            baseline_sample.scenario,
                            baseline_sample.sample
                        )
                    })?;
            Ok((baseline_sample, candidate_sample))
        })
        .collect()
}

fn bootstrap_reduction(
    pairs: &[(&AdapterMetrics, &AdapterMetrics)],
    metric: impl Fn(&AdapterMetrics) -> u64 + Copy,
    seed: u64,
) -> Estimate {
    let value = reduction(
        pairs.iter().map(|(_, candidate)| metric(candidate)).sum(),
        pairs.iter().map(|(baseline, _)| metric(baseline)).sum(),
    );
    let mut random = DeterministicRandom::new(seed);
    let mut estimates = Vec::with_capacity(BOOTSTRAP_RESAMPLES);
    for _ in 0..BOOTSTRAP_RESAMPLES {
        let mut baseline_total = 0_u64;
        let mut candidate_total = 0_u64;
        for _ in 0..pairs.len() {
            let (baseline, candidate) = pairs[random.index(pairs.len())];
            baseline_total = baseline_total.saturating_add(metric(baseline));
            candidate_total = candidate_total.saturating_add(metric(candidate));
        }
        estimates.push(reduction(candidate_total, baseline_total));
    }
    estimate_with_interval(value, estimates)
}

fn bootstrap_wall_overhead(pairs: &[(&AdapterMetrics, &AdapterMetrics)], seed: u64) -> Estimate {
    let paired_overheads: Vec<_> = pairs
        .iter()
        .map(|(baseline, candidate)| overhead(candidate.bazel_wall_ms, baseline.bazel_wall_ms))
        .collect();
    let value = median_f64(paired_overheads.clone());
    let mut random = DeterministicRandom::new(seed);
    let mut estimates = Vec::with_capacity(BOOTSTRAP_RESAMPLES);
    for _ in 0..BOOTSTRAP_RESAMPLES {
        let resample = (0..paired_overheads.len())
            .map(|_| paired_overheads[random.index(paired_overheads.len())])
            .collect();
        estimates.push(median_f64(resample));
    }
    estimate_with_interval(value, estimates)
}

fn estimate_with_interval(value: f64, mut estimates: Vec<f64>) -> Estimate {
    estimates.sort_by(f64::total_cmp);
    Estimate {
        value,
        ci95_lower: percentile_sorted_f64(&estimates, 25, 1_000),
        ci95_upper: percentile_sorted_f64(&estimates, 975, 1_000),
    }
}

fn percentile_sorted_f64(values: &[f64], numerator: usize, denominator: usize) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let index = (values.len() * numerator)
        .div_ceil(denominator)
        .saturating_sub(1)
        .min(values.len() - 1);
    values[index]
}

fn point_estimates(estimates: &BTreeMap<String, Estimate>) -> BTreeMap<String, f64> {
    estimates
        .iter()
        .map(|(metric, estimate)| (metric.clone(), estimate.value))
        .collect()
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
        (self.0 as usize) % length
    }
}

fn stable_seed(value: &str) -> u64 {
    value.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3)
    })
}

fn statistics(samples: &[AdapterMetrics]) -> Vec<SummaryStatistics> {
    let caches: BTreeSet<_> = samples
        .iter()
        .map(|sample| sample.cache_condition.as_str())
        .collect();
    let mut result = Vec::new();
    for cache in caches {
        for adapter in Adapter::ALL {
            let observations: Vec<_> = samples
                .iter()
                .filter(|sample| {
                    sample.cache_condition == cache && sample.adapter == adapter.name()
                })
                .collect();
            result.push(SummaryStatistics {
                cache_condition: cache.to_owned(),
                adapter: adapter.name().to_owned(),
                observations: observations.len(),
                median_bazel_wall_ms: median(
                    observations
                        .iter()
                        .map(|sample| sample.bazel_wall_ms)
                        .collect(),
                ),
                p95_bazel_wall_ms: percentile(
                    observations
                        .iter()
                        .map(|sample| sample.bazel_wall_ms)
                        .collect(),
                    95,
                ),
                median_context_tokens: median(
                    observations
                        .iter()
                        .map(|sample| sample.transcript.cumulative_context_tokens)
                        .collect(),
                ),
                p95_context_tokens: percentile(
                    observations
                        .iter()
                        .map(|sample| sample.transcript.cumulative_context_tokens)
                        .collect(),
                    95,
                ),
                median_visible_bytes: median(
                    observations
                        .iter()
                        .map(|sample| sample.transcript.model_visible_bytes)
                        .collect(),
                ),
                p95_visible_bytes: percentile(
                    observations
                        .iter()
                        .map(|sample| sample.transcript.model_visible_bytes)
                        .collect(),
                    95,
                ),
            });
        }
    }
    result
}

fn median(values: Vec<u64>) -> u64 {
    percentile(values, 50)
}

fn percentile(mut values: Vec<u64>, percentile: usize) -> u64 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    let index = (values.len() * percentile).div_ceil(100).saturating_sub(1);
    values[index]
}

fn median_f64(mut values: Vec<f64>) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    values[(values.len() - 1) / 2]
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

fn assert_gates(report: &BenchmarkReport, scenarios: &[Scenario]) -> anyhow::Result<()> {
    for required in [
        "build_success",
        "build_compile_failure",
        "build_noisy_failure",
        "test_success",
        "test_failure",
        "query",
    ] {
        ensure!(
            scenarios.iter().any(|scenario| scenario.name == required),
            "the acceptance run is missing required scenario {required}"
        );
    }
    for comparison in &report.comparisons {
        let context = &comparison.aggregate["cumulative_context_tokens"];
        let bytes = &comparison.aggregate["model_visible_bytes"];
        let overhead = &comparison.aggregate["bazel_wall_overhead"];
        ensure!(
            context.value >= 75.0 && context.ci95_lower >= 75.0,
            "context token reduction against {} is {:.2}% (95% CI lower {:.2}%), below 75%",
            comparison.baseline_adapter,
            context.value,
            context.ci95_lower
        );
        ensure!(
            bytes.value >= 75.0 && bytes.ci95_lower >= 75.0,
            "visible byte reduction against {} is {:.2}% (95% CI lower {:.2}%), below 75%",
            comparison.baseline_adapter,
            bytes.value,
            bytes.ci95_lower
        );
        ensure!(
            overhead.value <= 3.0,
            "Bazel wall-time overhead against {} is {:.2}%, above 3%",
            comparison.baseline_adapter,
            overhead.value
        );
    }
    ensure!(
        report.samples.iter().all(|sample| sample.diagnostic_found),
        "at least one expected root cause was absent from model-visible output"
    );
    Ok(())
}

fn reduction(candidate: u64, baseline: u64) -> f64 {
    if baseline == 0 {
        0.0
    } else {
        100.0 * (1.0 - candidate as f64 / baseline as f64)
    }
}

fn overhead(candidate: u64, baseline: u64) -> f64 {
    if baseline == 0 {
        0.0
    } else {
        100.0 * (candidate as f64 / baseline as f64 - 1.0)
    }
}

fn canonicalize_output(value: &str, workspace: &Path, output_root: &Path) -> String {
    normalize_terminal_text(value.as_bytes())
        .replace(workspace.to_string_lossy().as_ref(), "<WORKSPACE>")
        .replace(output_root.to_string_lossy().as_ref(), "<OUTPUT_ROOT>")
}

async fn create_worktree(corpus: &Path, worktree: &Path, commit: &str) -> anyhow::Result<()> {
    if worktree.exists() {
        remove_worktree(corpus, worktree).await?;
    }
    let status = Command::new("git")
        .args(["-C"])
        .arg(corpus)
        .args(["worktree", "add", "--detach", "--force"])
        .arg(worktree)
        .arg(commit)
        .status()
        .await?;
    ensure!(status.success(), "could not create benchmark worktree");
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
    ensure!(status.success(), "could not remove benchmark worktree");
    Ok(())
}

async fn install_overlay(repository_root: &Path, worktree: &Path) -> anyhow::Result<()> {
    let source = repository_root.join("crates/bazel-mcp-benchmark/resources/scenarios/abseil-cpp");
    let destination = worktree.join("bazel_mcp_token_bench");
    tokio::task::spawn_blocking(move || copy_directory(&source, &destination)).await??;
    Ok(())
}

async fn find_test_log(output_root: &Path) -> anyhow::Result<Option<String>> {
    let output_root = output_root.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut pending = vec![output_root];
        while let Some(directory) = pending.pop() {
            let entries = match std::fs::read_dir(directory) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            for entry in entries {
                let entry = entry?;
                let file_type = entry.file_type()?;
                if file_type.is_dir() {
                    pending.push(entry.path());
                } else if entry.file_name() == "test.log" {
                    return Ok(Some(
                        String::from_utf8_lossy(&std::fs::read(entry.path())?).into(),
                    ));
                }
            }
        }
        Ok(None)
    })
    .await?
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

fn bazel_executable() -> anyhow::Result<PathBuf> {
    if let Some(path) = std::env::var_os("BAZEL_MCP_BAZEL") {
        return Ok(PathBuf::from(path));
    }
    which_on_path("bazelisk")
        .or_else(|| which_on_path("bazel"))
        .context("bazelisk or bazel is required")
}

fn which_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
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
