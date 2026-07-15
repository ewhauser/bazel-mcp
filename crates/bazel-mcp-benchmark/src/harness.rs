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
    AdapterMetrics, BenchmarkReport, EnvironmentMetadata, ProjectManifest, Scenario,
    SummaryStatistics, Transcript, TranscriptEvent, TranscriptKind,
};

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
                    tokio::fs::write(&evidence_path, &observation.raw_evidence).await?;
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

    let aggregate_reduction_percent = aggregate(&samples, None)?;
    let reduction_percent_by_cache = config
        .cache_conditions
        .iter()
        .map(|cache| Ok((cache.clone(), aggregate(&samples, Some(cache))?)))
        .collect::<anyhow::Result<BTreeMap<_, _>>>()?;
    let statistics = statistics(&samples);
    let report = BenchmarkReport {
        schema_version: 2,
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

fn aggregate(
    samples: &[AdapterMetrics],
    cache_condition: Option<&str>,
) -> anyhow::Result<BTreeMap<String, f64>> {
    let baseline: Vec<_> = samples
        .iter()
        .filter(|sample| {
            sample.adapter == Adapter::ShellDefault.name()
                && cache_condition.is_none_or(|cache| sample.cache_condition == cache)
        })
        .collect();
    let candidate: Vec<_> = samples
        .iter()
        .filter(|sample| {
            sample.adapter == Adapter::BazelMcp.name()
                && cache_condition.is_none_or(|cache| sample.cache_condition == cache)
        })
        .collect();
    ensure!(
        !baseline.is_empty() && baseline.len() == candidate.len(),
        "incomplete adapter samples"
    );
    let baseline_context: u64 = baseline
        .iter()
        .map(|sample| sample.transcript.cumulative_context_tokens)
        .sum();
    let candidate_context: u64 = candidate
        .iter()
        .map(|sample| sample.transcript.cumulative_context_tokens)
        .sum();
    let baseline_visible: u64 = baseline
        .iter()
        .map(|sample| sample.transcript.model_visible_bytes)
        .sum();
    let candidate_visible: u64 = candidate
        .iter()
        .map(|sample| sample.transcript.model_visible_bytes)
        .sum();
    let baseline_tool: u64 = baseline
        .iter()
        .map(|sample| sample.transcript.visible_tool_tokens)
        .sum();
    let candidate_tool: u64 = candidate
        .iter()
        .map(|sample| sample.transcript.visible_tool_tokens)
        .sum();
    let baseline_wall: BTreeMap<_, _> = baseline
        .iter()
        .map(|sample| {
            (
                (
                    sample.cache_condition.as_str(),
                    sample.scenario.as_str(),
                    sample.sample,
                ),
                sample.bazel_wall_ms,
            )
        })
        .collect();
    let paired_overheads = candidate
        .iter()
        .filter_map(|sample| {
            let baseline = baseline_wall.get(&(
                sample.cache_condition.as_str(),
                sample.scenario.as_str(),
                sample.sample,
            ))?;
            Some(overhead(sample.bazel_wall_ms, *baseline))
        })
        .collect();
    Ok(BTreeMap::from([
        (
            "cumulative_context_tokens".to_owned(),
            reduction(candidate_context, baseline_context),
        ),
        (
            "model_visible_bytes".to_owned(),
            reduction(candidate_visible, baseline_visible),
        ),
        (
            "visible_tool_tokens".to_owned(),
            reduction(candidate_tool, baseline_tool),
        ),
        (
            "bazel_wall_overhead".to_owned(),
            median_f64(paired_overheads),
        ),
    ]))
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
    ensure!(
        scenarios.iter().any(|scenario| scenario.command == "build")
            && scenarios.iter().any(|scenario| scenario.command == "test"),
        "the acceptance run must include both Bazel build and test"
    );
    let context = report.aggregate_reduction_percent["cumulative_context_tokens"];
    let bytes = report.aggregate_reduction_percent["model_visible_bytes"];
    let overhead = report.aggregate_reduction_percent["bazel_wall_overhead"];
    ensure!(
        context >= 75.0,
        "context token reduction {context:.2}% is below 75%"
    );
    ensure!(
        bytes >= 75.0,
        "visible byte reduction {bytes:.2}% is below 75%"
    );
    ensure!(
        overhead <= 3.0,
        "Bazel wall-time overhead {overhead:.2}% exceeds 3%"
    );
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
