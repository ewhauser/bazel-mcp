#![cfg(unix)]

use std::{os::unix::fs::PermissionsExt, path::Path, time::Duration};

use nix::{
    sys::signal::{Signal, kill},
    unistd::Pid,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
};

async fn wait_for_path(path: &Path) {
    for _ in 0..1_000 {
        if path.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {}", path.display());
}

async fn initialize(server: &mut Child, client_name: &str) {
    let request = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": client_name, "version": "1"}
        }
    });
    let stdin = server.stdin.as_mut().unwrap();
    stdin
        .write_all(format!("{request}\n").as_bytes())
        .await
        .unwrap();
    stdin.flush().await.unwrap();
    let mut line = String::new();
    tokio::time::timeout(
        Duration::from_secs(10),
        BufReader::new(server.stdout.as_mut().unwrap()).read_line(&mut line),
    )
    .await
    .expect("server did not initialize before timeout")
    .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&line)
            .unwrap()
            .get("id"),
        Some(&serde_json::json!(1))
    );
}

#[tokio::test]
async fn two_servers_start_with_one_shared_cache_root() {
    let root = tempfile::tempdir().unwrap();
    let cache_root = root.path().join("store");
    let fake_bazel = root.path().join("fake-bazel");
    tokio::fs::write(
        &fake_bazel,
        "#!/bin/sh\nif [ \"${1:-}\" = --version ]; then echo 'bazel 9.1.0'; exit 0; fi\nexit 0\n",
    )
    .await
    .unwrap();
    tokio::fs::set_permissions(&fake_bazel, std::fs::Permissions::from_mode(0o700))
        .await
        .unwrap();

    let mut configs = Vec::new();
    for name in ["a", "b"] {
        let workspace = root.path().join(format!("worktree-{name}"));
        tokio::fs::create_dir(&workspace).await.unwrap();
        tokio::fs::write(
            workspace.join("MODULE.bazel"),
            format!("module(name='{name}')\n"),
        )
        .await
        .unwrap();
        let config = root.path().join(format!("config-{name}.toml"));
        tokio::fs::write(
            &config,
            format!(
                "allowed_roots = [{workspace:?}]\ncache_root = {cache_root:?}\nbazel_executable = {fake_bazel:?}\n"
            ),
        )
        .await
        .unwrap();
        configs.push(config);
    }

    let spawn = |config: &Path| {
        Command::new(env!("CARGO_BIN_EXE_bazel-mcp"))
            .args(["--config", config.to_str().unwrap(), "--log", "error"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap()
    };
    let mut first = spawn(&configs[0]);
    initialize(&mut first, "shared-cache-a").await;
    let mut second = spawn(&configs[1]);
    initialize(&mut second, "shared-cache-b").await;
    assert!(first.try_wait().unwrap().is_none());
    assert!(second.try_wait().unwrap().is_none());
    first.start_kill().unwrap();
    second.start_kill().unwrap();
    let _ = first.wait().await;
    let _ = second.wait().await;
}

#[tokio::test]
async fn sigterm_gracefully_kills_the_active_bazel_process_group() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    tokio::fs::create_dir(&workspace).await.unwrap();
    tokio::fs::write(
        workspace.join("MODULE.bazel"),
        "module(name='shutdown_test')\n",
    )
    .await
    .unwrap();
    let grandchild_pid = root.path().join("grandchild.pid");
    let fake_bazel = root.path().join("fake-bazel");
    tokio::fs::write(
        &fake_bazel,
        format!(
            "#!/bin/sh\nif [ \"${{1:-}}\" = --version ]; then echo 'bazel 9.1.0'; exit 0; fi\n(sleep 60) &\necho $! > '{}'\nwait\n",
            grandchild_pid.display()
        ),
    )
    .await
    .unwrap();
    tokio::fs::set_permissions(&fake_bazel, std::fs::Permissions::from_mode(0o700))
        .await
        .unwrap();
    let config = root.path().join("config.toml");
    tokio::fs::write(
        &config,
        format!(
            "allowed_roots = [{:?}]\ncache_root = {:?}\nbazel_executable = {:?}\ncancellation_interrupt_grace_seconds = 1\ncancellation_terminate_grace_seconds = 1\n",
            workspace,
            root.path().join("store"),
            fake_bazel,
        ),
    )
    .await
    .unwrap();

    let mut server = Command::new(env!("CARGO_BIN_EXE_bazel-mcp"))
        .args(["--config", config.to_str().unwrap(), "--log", "error"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let server_pid = Pid::from_raw(i32::try_from(server.id().unwrap()).unwrap());
    let mut stdin = server.stdin.take().unwrap();
    let stdout = server.stdout.take().unwrap();
    let mut lines = BufReader::new(stdout).lines();
    stdin
        .write_all(
            format!(
                "{}\n{}\n{}\n",
                serde_json::json!({
                    "jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": {
                        "protocolVersion": "2025-06-18", "capabilities": {},
                        "clientInfo": {"name": "shutdown-test", "version": "1"}
                    }
                }),
                serde_json::json!({
                    "jsonrpc": "2.0", "method": "notifications/initialized"
                }),
                serde_json::json!({
                    "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                    "params": {"name": "bazel.run", "arguments": {
                        "workspace": workspace, "command": "build", "args": ["//:target"]
                    }}
                }),
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    stdin.flush().await.unwrap();
    let initialize = lines.next_line().await.unwrap().unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&initialize)
            .unwrap()
            .get("id"),
        Some(&serde_json::json!(1))
    );

    wait_for_path(&grandchild_pid).await;
    let pid = Pid::from_raw(
        tokio::fs::read_to_string(&grandchild_pid)
            .await
            .unwrap()
            .trim()
            .parse()
            .unwrap(),
    );
    kill(server_pid, Signal::SIGTERM).unwrap();
    let status = match tokio::time::timeout(Duration::from_secs(10), server.wait()).await {
        Ok(status) => status.unwrap(),
        Err(_) => {
            server.start_kill().unwrap();
            let _ = server.wait().await;
            panic!("server did not stop after SIGTERM");
        }
    };
    assert!(status.success());
    for _ in 0..500 {
        if kill(pid, None).is_err() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("Bazel grandchild {pid} survived server shutdown");
}
