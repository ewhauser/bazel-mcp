use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};

use anyhow::{Context, Result, bail, ensure};
use bazel_mcp_store::InvocationPaths;
use bazel_mcp_types::{Artifact, Diagnostic, InvocationId};
use serde::Serialize;
use serde_json::{Value, json};
use tempfile::TempDir;
use uuid::Uuid;

use crate::{CaseObservation, LoadedCase};

#[derive(Clone, Debug)]
pub struct LiveOptions {
    pub repository_root: PathBuf,
    pub server: PathBuf,
    pub bazel_executable: Option<PathBuf>,
    pub runtime_parent: Option<PathBuf>,
    pub bazel_version: Option<String>,
}

#[derive(Debug)]
pub struct LiveRun {
    pub observation: CaseObservation,
    pub invocation_id: String,
    pub invocation_paths: InvocationPaths,
    pub workspace: PathBuf,
    pub cache_root: PathBuf,
    pub output_user_root: PathBuf,
    pub run_result: Value,
    pub bazel_version: Option<String>,
    runtime: TempDir,
}

impl LiveRun {
    #[must_use]
    pub fn runtime_root(&self) -> &Path {
        self.runtime.path()
    }
}

#[derive(Serialize)]
struct HarnessConfig<'a> {
    allowed_roots: [&'a Path; 1],
    cache_root: &'a Path,
    output_user_root: &'a Path,
    #[serde(skip_serializing_if = "Option::is_none")]
    bazel_executable: Option<&'a Path>,
    environment_allowlist: [&'static str; 2],
    redaction_patterns: [&'static str; 3],
    result_encoding: &'static str,
    bep_transport: &'static str,
    mcp_execution_policy: &'static str,
    default_timeout_seconds: u64,
    maximum_timeout_seconds: u64,
    progress_initial_seconds: u64,
    progress_interval_seconds: u64,
}

pub fn run_live_case(case: &LoadedCase, options: &LiveOptions) -> Result<LiveRun> {
    ensure!(
        case.manifest.supports_current_platform(),
        "case {} does not support this platform",
        case.manifest.id
    );
    let workspace = options.repository_root.join(&case.manifest.workspace);
    let workspace = workspace
        .canonicalize()
        .with_context(|| format!("canonicalize example workspace {}", workspace.display()))?;
    let runtime = if let Some(parent) = &options.runtime_parent {
        fs::create_dir_all(parent)
            .with_context(|| format!("create runtime parent {}", parent.display()))?;
        tempfile::Builder::new()
            .prefix("reducer-case-")
            .tempdir_in(parent)
            .with_context(|| format!("create runtime under {}", parent.display()))?
    } else {
        tempfile::Builder::new()
            .prefix("bazel-mcp-reducer-case-")
            .tempdir()
            .context("create reducer case runtime")?
    };
    let cache_root = runtime.path().join("store");
    let output_user_root = runtime.path().join("bazel");
    fs::create_dir_all(&cache_root)?;
    fs::create_dir_all(&output_user_root)?;
    let config_path = runtime.path().join("config.toml");
    let config = HarnessConfig {
        allowed_roots: [&workspace],
        cache_root: &cache_root,
        output_user_root: &output_user_root,
        bazel_executable: options.bazel_executable.as_deref(),
        environment_allowlist: ["BAZELISK_HOME", "USE_BAZEL_VERSION"],
        redaction_patterns: [
            "(?i)token=[^\\s]+",
            "(?i)(authorization|x-buildbuddy-api-key)=[^\\s]+",
            "(?i)[?&](x-amz-signature|x-goog-signature)=[^&\\s]+",
        ],
        result_encoding: "structured",
        bep_transport: "tail",
        mcp_execution_policy: "sync_only",
        default_timeout_seconds: case.manifest.timeout_seconds,
        maximum_timeout_seconds: case.manifest.timeout_seconds,
        progress_initial_seconds: 3600,
        progress_interval_seconds: 3600,
    };
    fs::write(&config_path, toml::to_string_pretty(&config)?)
        .with_context(|| format!("write harness config {}", config_path.display()))?;

    let mut command = Command::new(&options.server);
    command
        .arg("--config")
        .arg(&config_path)
        .arg("--log")
        .arg("error")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .env("BAZELISK_HOME", runtime.path().join("bazelisk"));
    let bazel_version = options
        .bazel_version
        .clone()
        .or_else(|| case.manifest.bazel_version.clone());
    if let Some(version) = &bazel_version {
        command.env("USE_BAZEL_VERSION", version);
    }
    let child = command
        .spawn()
        .with_context(|| format!("start MCP server {}", options.server.display()))?;
    let mut client = McpClient::new(child)?;
    client.initialize()?;
    let run_result = client.call_tool(
        "bazel.run",
        json!({
            "workspace": &workspace,
            "command": &case.manifest.command,
            "args": &case.manifest.args,
            "startup_args": &case.manifest.startup_args,
            "timeout_seconds": case.manifest.timeout_seconds,
        }),
    )?;
    let invocation_id = required_string(&run_result, "invocation_id")?.to_owned();
    let artifact_result = client.call_tool(
        "bazel.inspect",
        json!({
            "invocation_id": &invocation_id,
            "view": "artifacts",
            "limit": 100,
            "max_bytes": 8192,
        }),
    )?;
    if case
        .manifest
        .evidence
        .as_ref()
        .is_some_and(|evidence| !evidence.test_logs.is_empty())
    {
        client.call_tool(
            "bazel.inspect",
            json!({
                "invocation_id": &invocation_id,
                "view": "test_log",
                "limit": 100,
                "max_bytes": 8192,
            }),
        )?;
    }
    client.stop();

    let diagnostics = run_result
        .get("diagnostics")
        .cloned()
        .map(serde_json::from_value::<Vec<Diagnostic>>)
        .transpose()
        .context("decode live diagnostics")?
        .unwrap_or_default();
    let artifacts = artifact_result
        .get("items")
        .cloned()
        .map(serde_json::from_value::<Vec<Artifact>>)
        .transpose()
        .context("decode live artifacts")?
        .unwrap_or_default();
    let id = InvocationId::from_uuid(
        Uuid::parse_str(&invocation_id).context("parse live invocation UUID")?,
    );
    let invocation_paths = InvocationPaths::new(&cache_root, id);
    let raw_text = read_retained_text(&invocation_paths)?;
    let observation = CaseObservation {
        state: required_string(&run_result, "state")?.to_owned(),
        exit_code: run_result
            .get("exit_code")
            .and_then(Value::as_i64)
            .and_then(|value| i32::try_from(value).ok()),
        headline: required_string(&run_result, "headline")?.to_owned(),
        inspect_hint: run_result
            .get("inspect_hint")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        diagnostics,
        artifacts,
        visible_bytes: serde_json::to_vec(&run_result)?.len(),
        raw_text,
    };
    Ok(LiveRun {
        observation,
        invocation_id,
        invocation_paths,
        workspace,
        cache_root,
        output_user_root,
        run_result,
        bazel_version,
        runtime,
    })
}

fn required_string<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .with_context(|| format!("structured MCP result has no string field {field:?}"))
}

fn read_retained_text(paths: &InvocationPaths) -> Result<String> {
    let mut output = String::new();
    for path in [&paths.stdout, &paths.stderr, &paths.test_logs_raw] {
        match fs::read(path) {
            Ok(bytes) => output.push_str(&String::from_utf8_lossy(&bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("read retained evidence {}", path.display()));
            }
        }
    }
    Ok(output)
}

struct McpClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl McpClient {
    fn new(mut child: Child) -> Result<Self> {
        let stdin = child.stdin.take().context("MCP server has no stdin")?;
        let stdout = child.stdout.take().context("MCP server has no stdout")?;
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
        })
    }

    fn initialize(&mut self) -> Result<()> {
        self.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "reducer-cases", "version": env!("CARGO_PKG_VERSION")},
            }),
        )?;
        self.send(json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
        }))?;
        Ok(())
    }

    fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value> {
        let result = self.request("tools/call", json!({"name": name, "arguments": arguments}))?;
        result
            .get("structuredContent")
            .cloned()
            .with_context(|| format!("{name} returned no structuredContent: {result}"))
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))?;
        loop {
            let mut line = String::new();
            let read = self.stdout.read_line(&mut line)?;
            if read == 0 {
                bail!("MCP server closed stdout while waiting for request {id}");
            }
            let message: Value = serde_json::from_str(&line)
                .with_context(|| format!("decode MCP message {line:?}"))?;
            if message.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                bail!("MCP request {method} failed: {error}");
            }
            return message
                .get("result")
                .cloned()
                .with_context(|| format!("MCP response has no result: {message}"));
        }
    }

    fn send(&mut self, message: Value) -> Result<()> {
        serde_json::to_writer(&mut self.stdin, &message)?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        Ok(())
    }

    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        self.stop();
    }
}
