//! Ephemeral CLI frontend that presents the bounded `bazel.run` result.

use std::{
    collections::BTreeSet,
    ffi::{OsStr, OsString},
    io::Write,
    path::{Path, PathBuf},
    process::{ExitCode, ExitStatus, Stdio},
};

use anyhow::{Context, bail};
use bazel_mcp_policy::{PolicyConfig, resolve_bazel_executable_excluding};
use bazel_mcp_runner::{InvocationService, RunnerConfig};
use bazel_mcp_store::Store;
use bazel_mcp_types::{BazelCommand, InvocationRecord, InvocationRequest, Termination};
use tempfile::Builder;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::{
    Cli, McpConfig, ValidatedServerConfig,
    result::{ResultEncoder, RunResultBuilder},
};

const AGENT_MODE_ENV: &str = "BAZEL_MCP_MODE";
const NO_AGENT_MODE: &str = "--no-agent-mode";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LaunchMode {
    Mcp,
    Agent(Vec<OsString>),
}

#[must_use]
pub fn detect_launch<I>(arguments: I) -> LaunchMode
where
    I: IntoIterator<Item = OsString>,
{
    detect_launch_with_mode(arguments, std::env::var_os(AGENT_MODE_ENV).as_deref())
}

fn detect_launch_with_mode<I>(arguments: I, environment_mode: Option<&OsStr>) -> LaunchMode
where
    I: IntoIterator<Item = OsString>,
{
    let mut arguments = arguments.into_iter();
    let executable = arguments.next().unwrap_or_default();
    let mut remaining = arguments.collect::<Vec<_>>();
    let shim_name = Path::new(&executable)
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| matches!(name, "bazel" | "bazel.exe"));
    let environment_mode = environment_mode
        .and_then(OsStr::to_str)
        .is_some_and(|mode| mode == "agent");
    let explicit_mode = remaining
        .first()
        .is_some_and(|argument| argument == "passthrough");

    if !(shim_name || environment_mode || explicit_mode) {
        return LaunchMode::Mcp;
    }
    if explicit_mode {
        remaining.remove(0);
        if remaining.first().is_some_and(|argument| argument == "--") {
            remaining.remove(0);
        }
    }
    LaunchMode::Agent(remaining)
}

#[must_use]
pub fn agent_log_filter() -> String {
    std::env::var("BAZEL_MCP_LOG").unwrap_or_else(|_| "bazel_mcp=warn".to_owned())
}

pub async fn run_agent(arguments: Vec<OsString>) -> ExitCode {
    match run_agent_inner(arguments).await {
        Ok(result) => {
            if let Some(output) = result.output {
                let mut stdout = std::io::stdout().lock();
                if let Err(error) = writeln!(stdout, "{output}") {
                    eprintln!("bazel-mcp agent mode failed: could not write result: {error}");
                    return ExitCode::FAILURE;
                }
            }
            process_exit_code(result.exit_code)
        }
        Err(error) => {
            eprintln!("bazel-mcp agent mode failed: {error:#}");
            ExitCode::from(2)
        }
    }
}

struct AgentResult {
    output: Option<String>,
    exit_code: i32,
}

async fn run_agent_inner(mut arguments: Vec<OsString>) -> anyhow::Result<AgentResult> {
    let raw = arguments
        .first()
        .is_some_and(|argument| argument == NO_AGENT_MODE);
    if raw {
        arguments.remove(0);
    }

    let launch_directory = std::env::current_dir().context("read current directory")?;
    let workspace = discover_workspace(&launch_directory);
    let cli = agent_cli();
    let mut config = ValidatedServerConfig::load(&cli)?;
    let policy = PolicyConfig::from(&config);
    let lookup_directory = workspace.as_deref().unwrap_or(&launch_directory);
    let current_executable = std::env::current_exe().context("resolve current executable")?;
    let bazel_executable =
        resolve_bazel_executable_excluding(lookup_directory, &policy, Some(&current_executable))?;

    if raw {
        return run_unfiltered(&bazel_executable, &launch_directory, &arguments).await;
    }

    let workspace = workspace.ok_or_else(|| {
        anyhow::anyhow!(
            "could not find MODULE.bazel, WORKSPACE.bazel, or WORKSPACE in {} or its ancestors",
            launch_directory.display()
        )
    })?;
    let parsed = parse_bazel_arguments(&arguments, &policy)?;
    let scratch = Builder::new()
        .prefix("bazel-mcp-agent-")
        .tempdir()
        .context("create ephemeral invocation store")?;
    config = config.with_agent_runtime(scratch.path().to_owned(), bazel_executable);
    let result_encoding = McpConfig::from(&config).result_encoding;
    let store = Store::open(config.cache_root())
        .await
        .context("open ephemeral invocation store")?;
    let runner = InvocationService::start(store.clone(), RunnerConfig::from(&config)).await?;
    let mut request = InvocationRequest::new(workspace, parsed.command, parsed.arguments);
    request.startup_arguments = parsed.startup_arguments;
    request.target = parsed.target;
    request.program_arguments = parsed.program_arguments;
    let cancellation = CancellationToken::new();
    let signal_cancellation = cancellation.clone();
    let signal = tokio::spawn(async move {
        crate::shutdown_signal().await;
        signal_cancellation.cancel();
    });
    let record = runner.run_with_cancellation(request, cancellation).await;
    signal.abort();
    let record = record.context("run filtered Bazel invocation")?;
    let output = RunResultBuilder::new(ResultEncoder::new(result_encoding))
        .build_cli(&record)
        .map_err(anyhow::Error::msg)?;
    let exit_code = invocation_exit_code(&record);
    drop(runner);
    drop(store);
    scratch
        .close()
        .context("remove ephemeral invocation store")?;
    Ok(AgentResult {
        output: Some(output),
        exit_code,
    })
}

fn agent_cli() -> Cli {
    Cli {
        config: std::env::var_os("BAZEL_MCP_CONFIG").map(PathBuf::from),
        allowed_roots: Vec::new(),
        cache_root: None,
        log: "bazel_mcp=warn".to_owned(),
    }
}

struct ParsedBazelArguments {
    startup_arguments: Vec<String>,
    command: BazelCommand,
    arguments: Vec<String>,
    target: Option<String>,
    program_arguments: Vec<String>,
}

fn parse_bazel_arguments(
    arguments: &[OsString],
    policy: &PolicyConfig,
) -> anyhow::Result<ParsedBazelArguments> {
    if arguments.is_empty() {
        return Ok(ParsedBazelArguments {
            startup_arguments: Vec::new(),
            command: BazelCommand::Help,
            arguments: Vec::new(),
            target: None,
            program_arguments: Vec::new(),
        });
    }
    if arguments.len() == 1 && matches!(arguments[0].to_str(), Some("--version" | "--help")) {
        return Ok(ParsedBazelArguments {
            startup_arguments: Vec::new(),
            command: if arguments[0] == "--version" {
                BazelCommand::Version
            } else {
                BazelCommand::Help
            },
            arguments: Vec::new(),
            target: None,
            program_arguments: Vec::new(),
        });
    }

    let command_names = policy
        .allowed_commands
        .union(&policy.denied_commands)
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let command_index = arguments.iter().position(|argument| {
        argument
            .to_str()
            .is_some_and(|argument| command_names.contains(argument))
    });
    let Some(command_index) = command_index else {
        bail!(
            "could not identify a configured Bazel command in argv; use --no-agent-mode for unsupported Bazel syntax"
        );
    };
    let strings = arguments
        .iter()
        .map(|argument| {
            argument.to_str().map(str::to_owned).ok_or_else(|| {
                anyhow::anyhow!(
                    "agent mode accepts only UTF-8 Bazel arguments; use --no-agent-mode for arbitrary platform arguments"
                )
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let command = strings[command_index]
        .parse::<BazelCommand>()
        .expect("BazelCommand parsing is infallible");
    let mut command_arguments = strings[command_index + 1..].to_vec();
    let mut target = None;
    let mut program_arguments = Vec::new();
    if command == BazelCommand::Run {
        if let Some(delimiter) = command_arguments
            .iter()
            .position(|argument| argument == "--")
        {
            program_arguments = command_arguments.split_off(delimiter + 1);
            command_arguments.pop();
        }
        if let Some(target_index) = command_arguments
            .iter()
            .position(|argument| !argument.starts_with('-'))
        {
            target = Some(command_arguments.remove(target_index));
        }
    }
    Ok(ParsedBazelArguments {
        startup_arguments: strings[..command_index].to_vec(),
        command,
        arguments: command_arguments,
        target,
        program_arguments,
    })
}

fn discover_workspace(start: &Path) -> Option<PathBuf> {
    let start = start.canonicalize().ok()?;
    start.ancestors().find_map(|directory| {
        ["MODULE.bazel", "WORKSPACE.bazel", "WORKSPACE"]
            .iter()
            .any(|marker| directory.join(marker).is_file())
            .then(|| directory.to_owned())
    })
}

async fn run_unfiltered(
    executable: &Path,
    launch_directory: &Path,
    arguments: &[OsString],
) -> anyhow::Result<AgentResult> {
    let status = Command::new(executable)
        .args(arguments)
        .current_dir(launch_directory)
        .env_remove(AGENT_MODE_ENV)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .with_context(|| format!("run unfiltered Bazel at {}", executable.display()))?;
    Ok(AgentResult {
        output: None,
        exit_code: status_exit_code(status),
    })
}

fn invocation_exit_code(record: &InvocationRecord) -> i32 {
    match record.termination.as_ref() {
        Some(Termination::Exit { code }) => *code,
        Some(Termination::Signal { signal }) => 128_i32.saturating_add(*signal),
        Some(Termination::Timeout) => 124,
        Some(Termination::Cancelled) => 130,
        Some(
            Termination::OutputLimit { .. }
            | Termination::SpawnFailure { .. }
            | Termination::Interrupted,
        )
        | None => 1,
    }
}

fn status_exit_code(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status.signal().map_or(1, |signal| 128 + signal)
    }
    #[cfg(not(unix))]
    1
}

fn process_exit_code(code: i32) -> ExitCode {
    u8::try_from(code)
        .map(ExitCode::from)
        .unwrap_or(ExitCode::FAILURE)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    fn os(arguments: &[&str]) -> Vec<OsString> {
        arguments.iter().map(OsString::from).collect()
    }

    #[test]
    fn detects_each_agent_mode_entrypoint() {
        assert_eq!(
            detect_launch_with_mode(os(&["/tmp/bazel", "test", "//..."]), None),
            LaunchMode::Agent(os(&["test", "//..."]))
        );
        assert!(matches!(
            detect_launch_with_mode(os(&["/tmp/bazel.exe", "test", "//..."]), None),
            LaunchMode::Agent(_)
        ));
        assert_eq!(
            detect_launch_with_mode(
                os(&["bazel-mcp", "build", "//..."]),
                Some(OsStr::new("agent"))
            ),
            LaunchMode::Agent(os(&["build", "//..."]))
        );
        assert_eq!(
            detect_launch_with_mode(
                os(&["bazel-mcp", "passthrough", "--", "query", "//..."]),
                None
            ),
            LaunchMode::Agent(os(&["query", "//..."]))
        );
        assert_eq!(
            detect_launch_with_mode(os(&["bazel-mcp", "--config", "config.toml"]), None),
            LaunchMode::Mcp
        );
    }

    #[test]
    fn parses_startup_arguments_around_a_configured_command() {
        let parsed = parse_bazel_arguments(
            &os(&[
                "--output_base",
                "/tmp/output",
                "test",
                "//pkg:all",
                "--test_filter=one",
            ]),
            &PolicyConfig::default(),
        )
        .unwrap();

        assert_eq!(parsed.startup_arguments, ["--output_base", "/tmp/output"]);
        assert_eq!(parsed.command, BazelCommand::Test);
        assert_eq!(parsed.arguments, ["//pkg:all", "--test_filter=one"]);
        assert_eq!(parsed.target, None);
        assert!(parsed.program_arguments.is_empty());
    }

    #[test]
    fn separates_run_target_and_private_program_arguments() {
        let mut policy = PolicyConfig::default();
        policy.allowed_commands.insert("run".to_owned());
        policy.denied_commands.remove("run");
        let parsed = parse_bazel_arguments(
            &os(&[
                "run",
                "--config=dev",
                "//cmd:example",
                "--",
                "--token=secret",
                "input.txt",
            ]),
            &policy,
        )
        .unwrap();

        assert_eq!(parsed.command, BazelCommand::Run);
        assert_eq!(parsed.arguments, ["--config=dev"]);
        assert_eq!(parsed.target.as_deref(), Some("//cmd:example"));
        assert_eq!(parsed.program_arguments, ["--token=secret", "input.txt"]);
    }

    #[test]
    fn translates_common_commandless_forms() {
        let empty = parse_bazel_arguments(&[], &PolicyConfig::default()).unwrap();
        assert_eq!(empty.command, BazelCommand::Help);
        let version = parse_bazel_arguments(&os(&["--version"]), &PolicyConfig::default()).unwrap();
        assert_eq!(version.command, BazelCommand::Version);
        let help = parse_bazel_arguments(&os(&["--help"]), &PolicyConfig::default()).unwrap();
        assert_eq!(help.command, BazelCommand::Help);
    }

    #[test]
    fn discovers_the_nearest_workspace_from_a_subdirectory() {
        let root = tempdir().unwrap();
        let nested = root.path().join("one/two");
        fs::create_dir_all(&nested).unwrap();
        fs::write(root.path().join("MODULE.bazel"), "").unwrap();

        assert_eq!(
            discover_workspace(&nested),
            Some(root.path().canonicalize().unwrap())
        );
    }
}
