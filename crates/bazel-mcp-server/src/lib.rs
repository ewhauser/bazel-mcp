//! Thin stdio MCP boundary for the Bazel invocation service.

mod config;
mod handler;

pub use bazel_mcp_runner::BepTransport;
pub use config::{Cli, McpExecutionPolicy, ResultEncoding, ServerConfig, StarlarkConfig};
pub use handler::BazelMcpServer;

use anyhow::Context;
use bazel_mcp_policy::PolicyConfig;
use bazel_mcp_runner::{InvocationService, RunnerConfig};
use bazel_mcp_store::Store;
use rmcp::RoleServer;

pub async fn serve(config: ServerConfig) -> anyhow::Result<()> {
    let store = Store::open(&config.cache_root).await.with_context(|| {
        format!(
            "open shared invocation store at {}",
            config.cache_root.display()
        )
    })?;
    store
        .enforce_retention(
            std::time::Duration::from_secs(config.retention_days.saturating_mul(24 * 60 * 60)),
            config.maximum_storage_bytes,
        )
        .await
        .context("enforce invocation retention")?;
    let policy = PolicyConfig {
        allowed_roots: config.allowed_roots.clone(),
        allowed_commands: config.allowed_commands.clone(),
        denied_commands: config.denied_commands.clone(),
        environment_allowlist: config.environment_allowlist.clone(),
        redaction_patterns: config.redaction_patterns.clone(),
        bazel_executable: config.bazel_executable.clone(),
    };
    let runner = InvocationService::start(
        store.clone(),
        RunnerConfig {
            policy,
            default_timeout: std::time::Duration::from_secs(config.default_timeout_seconds),
            maximum_timeout: std::time::Duration::from_secs(config.maximum_timeout_seconds),
            cancellation_interrupt_grace: std::time::Duration::from_secs(
                config.cancellation_interrupt_grace_seconds,
            ),
            cancellation_terminate_grace: std::time::Duration::from_secs(
                config.cancellation_terminate_grace_seconds,
            ),
            global_concurrency: config.global_concurrency,
            output_user_root: config.output_user_root.clone(),
            isolated_bazel_server_idle_timeout: std::time::Duration::from_secs(
                config.isolated_bazel_server_idle_seconds,
            ),
            supported_bazel_major_versions: config.supported_bazel_major_versions.clone(),
            allow_unsupported_bazel_versions: config.allow_unsupported_bazel_versions,
            version_check_timeout: std::time::Duration::from_secs(
                config.version_check_timeout_seconds,
            ),
            maximum_pending_invocations: config.maximum_pending_invocations,
            output_base_lock_root: RunnerConfig::default().output_base_lock_root,
            bep_transport: config.bep_transport,
            starlark_reducers: config.starlark.runner_config(),
        },
    )
    .await?;
    let shutdown_runner = runner.clone();
    let server = BazelMcpServer::new(runner, config.result_encoding)
        .with_progress_timing(
            std::time::Duration::from_secs(config.progress_initial_seconds),
            std::time::Duration::from_secs(config.progress_interval_seconds),
        )
        .with_task_execution(
            config.mcp_execution_policy,
            std::time::Duration::from_secs(config.task_ttl_seconds),
            config.task_poll_interval_ms,
        );
    tracing::info!(
        policy = ?config.mcp_execution_policy,
        task_ttl_seconds = config.task_ttl_seconds,
        task_poll_interval_ms = config.task_poll_interval_ms,
        legacy_protocol = "2025-11-25",
        extension_protocol = "2026-06-30",
        "configured negotiated MCP task execution"
    );
    let shutdown_store = store.clone();
    let cleanup_store = store;
    let cleanup_age =
        std::time::Duration::from_secs(config.retention_days.saturating_mul(24 * 60 * 60));
    let cleanup_bytes = config.maximum_storage_bytes;
    let cleanup_interval =
        std::time::Duration::from_secs(config.retention_cleanup_interval_seconds);
    let cleanup_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(cleanup_interval);
        interval.tick().await;
        loop {
            interval.tick().await;
            match cleanup_store
                .enforce_retention(cleanup_age, cleanup_bytes)
                .await
            {
                Ok(deleted) if deleted > 0 => {
                    tracing::info!(deleted, "removed expired Bazel invocation evidence");
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(%error, "periodic invocation retention failed");
                }
            }
        }
    });
    let running = rmcp::service::serve_directly::<RoleServer, _, _, _, _>(
        server,
        rmcp::transport::stdio(),
        None,
    );
    let service_cancellation = running.cancellation_token();
    let shutdown_wait = config
        .cancellation_interrupt_grace_seconds
        .saturating_add(config.cancellation_terminate_grace_seconds)
        .saturating_add(5);
    let waiting = running.waiting();
    tokio::pin!(waiting);
    let result = tokio::select! {
        result = &mut waiting => result.context("run stdio MCP server").map(|_| ()),
        () = shutdown_signal() => {
            let active = shutdown_runner.cancel_all_active().await;
            tracing::info!(
                active,
                "received shutdown signal; cancelling active Bazel invocations"
            );
            service_cancellation.cancel();
            let deadline = tokio::time::Instant::now()
                + std::time::Duration::from_secs(shutdown_wait);
            while shutdown_runner.active_invocation_count().await > 0
                && tokio::time::Instant::now() < deadline
            {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            tracing::info!(
                active = shutdown_runner.active_invocation_count().await,
                "finished Bazel invocation shutdown wait"
            );
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                &mut waiting,
            )
            .await;
            Ok(())
        }
    };
    match shutdown_store.flush_pending_telemetry().await {
        Ok(flushed) if flushed > 0 => {
            tracing::debug!(flushed, "flushed pending invocation telemetry");
        }
        Ok(_) => {}
        Err(error) => {
            tracing::warn!(%error, "could not flush pending invocation telemetry");
        }
    }
    cleanup_task.abort();
    result
}

#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    let Ok(mut terminate) = signal(SignalKind::terminate()) else {
        let _ = tokio::signal::ctrl_c().await;
        return;
    };
    tokio::select! {
        result = tokio::signal::ctrl_c() => { let _ = result; }
        _ = terminate.recv() => {}
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
