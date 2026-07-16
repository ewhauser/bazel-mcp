use std::{
    env,
    fs::OpenOptions,
    io::Write,
    path::PathBuf,
    process::{Command, Stdio},
};

use anyhow::{Context, ensure};
use serde::Serialize;

#[derive(Serialize)]
struct Invocation<'a> {
    denied: bool,
    argv: &'a [String],
}

fn main() -> anyhow::Result<()> {
    let argv: Vec<String> = env::args_os()
        .skip(1)
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let denied = env::var_os("BAZEL_AGENTIC_DENY").is_some_and(|value| value != "0");
    append_invocation(denied, &argv)?;
    if denied {
        eprintln!("Direct shell Bazel invocation is disabled for the bazel-mcp benchmark adapter.");
        std::process::exit(64);
    }
    let executable = env::var_os("BAZEL_AGENTIC_REAL")
        .map(PathBuf::from)
        .context("BAZEL_AGENTIC_REAL is missing")?;
    let output_root = env::var_os("BAZEL_AGENTIC_OUTPUT_USER_ROOT")
        .map(PathBuf::from)
        .context("BAZEL_AGENTIC_OUTPUT_USER_ROOT is missing")?;
    ensure!(
        executable.is_file(),
        "configured Bazel executable is missing"
    );
    let status = Command::new(executable)
        .arg(format!("--output_user_root={}", output_root.display()))
        .args(env::args_os().skip(1))
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("launch instrumented Bazel process")?;
    std::process::exit(status.code().unwrap_or(1));
}

fn append_invocation(denied: bool, argv: &[String]) -> anyhow::Result<()> {
    let path = env::var_os("BAZEL_AGENTIC_LOG")
        .map(PathBuf::from)
        .context("BAZEL_AGENTIC_LOG is missing")?;
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .with_context(|| format!("open Bazel invocation log {}", path.display()))?;
    serde_json::to_writer(&mut file, &Invocation { denied, argv })?;
    file.write_all(b"\n")?;
    Ok(())
}
