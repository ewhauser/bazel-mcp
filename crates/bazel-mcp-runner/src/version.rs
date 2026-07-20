use std::{collections::BTreeMap, path::Path, process::Stdio, time::Duration};

use thiserror::Error;
use tokio::{io::AsyncReadExt, process::Command};

use crate::cancel::{ProcessGroupGuard, terminate_child};

const VERSION_OUTPUT_LIMIT: u64 = 64 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BazelVersion {
    raw: String,
    pub(crate) major: u32,
}

#[derive(Debug, Error)]
pub(crate) enum VersionError {
    #[error("could not start `{executable} --version`: {source}")]
    Spawn {
        executable: String,
        #[source]
        source: std::io::Error,
    },
    #[error("`{executable} --version` did not finish within {timeout_seconds} seconds")]
    Timeout {
        executable: String,
        timeout_seconds: u64,
    },
    #[error("`{executable} --version` failed with exit code {exit_code:?}: {output}")]
    Failed {
        executable: String,
        exit_code: Option<i32>,
        output: String,
    },
    #[error("could not parse a Bazel release from `{executable} --version`: {output}")]
    Unrecognized { executable: String, output: String },
    #[error("could not read `{executable} --version` output: {source}")]
    Read {
        executable: String,
        #[source]
        source: std::io::Error,
    },
}

pub(crate) async fn detect_bazel_version(
    executable: &Path,
    workspace: &Path,
    environment: &BTreeMap<String, String>,
    timeout: Duration,
    interrupt_grace: Duration,
    terminate_grace: Duration,
) -> Result<BazelVersion, VersionError> {
    let display = executable.display().to_string();
    let mut command = Command::new(executable);
    command
        .arg("--version")
        .current_dir(workspace)
        .env_clear()
        .envs(environment)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }

    let mut child = command.spawn().map_err(|source| VersionError::Spawn {
        executable: display.clone(),
        source,
    })?;
    let mut process_group = ProcessGroupGuard::for_child(&child);
    let stdout = child.stdout.take().expect("piped stdout is present");
    let stderr = child.stderr.take().expect("piped stderr is present");
    let stdout_task = tokio::spawn(read_bounded(stdout));
    let stderr_task = tokio::spawn(read_bounded(stderr));

    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(result) => result.map_err(|source| VersionError::Read {
            executable: display.clone(),
            source,
        })?,
        Err(_) => {
            let _ = terminate_child(&mut child, interrupt_grace, terminate_grace).await;
            stdout_task.abort();
            stderr_task.abort();
            return Err(VersionError::Timeout {
                executable: display,
                timeout_seconds: timeout.as_secs(),
            });
        }
    };
    process_group.disarm();
    let stdout = join_output(stdout_task, &display).await?;
    let stderr = join_output(stderr_task, &display).await?;
    let output = normalized_output(&stdout, &stderr);
    if !status.success() {
        return Err(VersionError::Failed {
            executable: display,
            exit_code: status.code(),
            output,
        });
    }
    parse_bazel_version(&output).ok_or(VersionError::Unrecognized {
        executable: display,
        output,
    })
}

async fn read_bounded<R>(reader: R) -> std::io::Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut bytes = Vec::new();
    reader
        .take(VERSION_OUTPUT_LIMIT)
        .read_to_end(&mut bytes)
        .await?;
    Ok(bytes)
}

async fn join_output(
    task: tokio::task::JoinHandle<std::io::Result<Vec<u8>>>,
    executable: &str,
) -> Result<Vec<u8>, VersionError> {
    match task.await {
        Ok(Ok(bytes)) => Ok(bytes),
        Ok(Err(source)) => Err(VersionError::Read {
            executable: executable.to_owned(),
            source,
        }),
        Err(error) => Err(VersionError::Read {
            executable: executable.to_owned(),
            source: std::io::Error::other(error.to_string()),
        }),
    }
}

fn normalized_output(stdout: &[u8], stderr: &[u8]) -> String {
    let bytes = if stdout.is_empty() { stderr } else { stdout };
    String::from_utf8_lossy(bytes)
        .trim()
        .chars()
        .take(2_000)
        .collect()
}

fn parse_bazel_version(output: &str) -> Option<BazelVersion> {
    for line in output.lines() {
        let candidate = line
            .strip_prefix("bazel ")
            .or_else(|| line.strip_prefix("Build label: "))
            .unwrap_or(line)
            .trim();
        let release = candidate.strip_prefix("release ").unwrap_or(candidate);
        let digits = release
            .trim_start_matches(|character: char| !character.is_ascii_digit())
            .split_once('.')
            .map_or(release, |(major, _)| major);
        if !digits.is_empty()
            && digits.chars().all(|character| character.is_ascii_digit())
            && let Ok(major) = digits.parse()
        {
            return Some(BazelVersion {
                raw: candidate.to_owned(),
                major,
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bazel_and_build_label_formats() {
        assert_eq!(
            parse_bazel_version("bazel 9.1.0"),
            Some(BazelVersion {
                raw: "9.1.0".to_owned(),
                major: 9,
            })
        );
        assert_eq!(
            parse_bazel_version("Build label: release 7.6.1\nBuild target: x"),
            Some(BazelVersion {
                raw: "release 7.6.1".to_owned(),
                major: 7,
            })
        );
        assert_eq!(parse_bazel_version("bazel development version"), None);
    }
}
