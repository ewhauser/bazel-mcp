#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::PathBuf,
    process::{Command, Output},
};

struct Fixture {
    _root: tempfile::TempDir,
    workspace: PathBuf,
    config: PathBuf,
    configured_cache: PathBuf,
    temporary_root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let nested = workspace.join("pkg/subdir");
        let fake_bazel = root.path().join("real-bazel");
        let config = root.path().join("config.toml");
        let configured_cache = root.path().join("configured-cache-must-stay-unused");
        let temporary_root = root.path().join("temporary");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir(&temporary_root).unwrap();
        fs::write(workspace.join("MODULE.bazel"), "module(name='agent_cli')\n").unwrap();
        fs::write(
            &fake_bazel,
            r#"#!/bin/sh
if [ "${1:-}" = "--version" ]; then
  printf 'bazel 9.1.0\n'
  exit 0
fi
printf 'RAW_STDOUT should not be replayed\n'
printf 'ERROR: RAW_SECRET synthetic compiler failure\n' >&2
exit 7
"#,
        )
        .unwrap();
        fs::set_permissions(&fake_bazel, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(
            &config,
            format!(
                "allowed_roots = [{workspace:?}]\ncache_root = {configured_cache:?}\nbazel_executable = {fake_bazel:?}\nresult_encoding = \"text\"\nredaction_patterns = [\"RAW_SECRET\"]\n"
            ),
        )
        .unwrap();
        Self {
            _root: root,
            workspace: nested,
            config,
            configured_cache,
            temporary_root,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(bazel_mcp_binary());
        command
            .current_dir(&self.workspace)
            .env("BAZEL_MCP_CONFIG", &self.config)
            .env("TMPDIR", &self.temporary_root)
            .env_remove("BAZEL_MCP_LOG");
        command
    }

    fn assert_ephemeral_storage_removed(&self) {
        assert!(
            !self.configured_cache.exists(),
            "agent mode used the configured persistent cache"
        );
        assert_eq!(fs::read_dir(&self.temporary_root).unwrap().count(), 0);
    }
}

fn assert_filtered(output: &Output) {
    assert_eq!(output.status.code(), Some(7));
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["state"], "failed");
    assert_eq!(value["exit_code"], 7);
    assert_eq!(value["available_views"], serde_json::json!([]));
    assert_eq!(value["more_available"], false);
    assert!(value.get("inspect_hint").is_none());
    assert_eq!(
        value["rerun_hint"],
        "rerun with --no-agent-mode for unfiltered Bazel output"
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("RAW_SECRET"));
    assert!(String::from_utf8_lossy(&output.stdout).contains("[REDACTED]"));
}

#[test]
fn environment_mode_returns_the_bounded_result_and_deletes_its_store() {
    let fixture = Fixture::new();
    let output = fixture
        .command()
        .env("BAZEL_MCP_MODE", "agent")
        .args(["build", "//:failure"])
        .output()
        .unwrap();

    assert_filtered(&output);
    fixture.assert_ephemeral_storage_removed();
}

#[test]
fn explicit_passthrough_mode_returns_the_bounded_result() {
    let fixture = Fixture::new();
    let output = fixture
        .command()
        .args(["passthrough", "--", "build", "//:failure"])
        .output()
        .unwrap();

    assert_filtered(&output);
    fixture.assert_ephemeral_storage_removed();
}

#[test]
fn bazel_filename_activates_agent_mode() {
    let fixture = Fixture::new();
    let shim = fixture.temporary_root.join("bazel");
    symlink(bazel_mcp_binary(), &shim).unwrap();
    let output = Command::new(&shim)
        .current_dir(&fixture.workspace)
        .env("BAZEL_MCP_CONFIG", &fixture.config)
        .env("TMPDIR", &fixture.temporary_root)
        .env_remove("BAZEL_MCP_MODE")
        .env_remove("BAZEL_MCP_LOG")
        .args(["build", "//:failure"])
        .output()
        .unwrap();

    assert_filtered(&output);
    fs::remove_file(&shim).unwrap();
    fixture.assert_ephemeral_storage_removed();
}

fn bazel_mcp_binary() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_BIN_EXE_bazel-mcp"));
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir().unwrap().join(path)
    };
    assert!(
        path.is_file(),
        "bazel-mcp test binary is missing at {}",
        path.display()
    );
    path
}

#[test]
fn no_agent_mode_inherits_raw_output_and_exit_status() {
    let fixture = Fixture::new();
    let output = fixture
        .command()
        .env("BAZEL_MCP_MODE", "agent")
        .args(["--no-agent-mode", "build", "//:failure"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(7));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "RAW_STDOUT should not be replayed\n"
    );
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "ERROR: RAW_SECRET synthetic compiler failure\n"
    );
    fixture.assert_ephemeral_storage_removed();
}
