//! Thin stdio MCP boundary for the Bazel invocation service.

mod agent;
mod config;
mod handler;
mod protocol;
mod result;
mod tasks;

pub use agent::{LaunchMode, agent_log_filter, detect_launch, run_agent};
pub use bazel_mcp_runner::BepTransport;
pub use config::{
    Cli, McpConfig, McpExecutionPolicy, RawAspectConfig, RawMcpConfig, RawPolicyConfig,
    RawRetentionConfig, RawRunnerConfig, RawServerConfig, RawStarlarkConfig, ResultEncoding,
    RetentionConfig, ValidatedServerConfig,
};
pub use handler::BazelMcpServer;

use anyhow::Context;
use bazel_mcp_runner::{InvocationService, RunnerConfig};
use bazel_mcp_store::Store;
use rmcp::RoleServer;

pub async fn serve(config: ValidatedServerConfig) -> anyhow::Result<()> {
    let store = Store::open(config.cache_root()).await.with_context(|| {
        format!(
            "open shared invocation store at {}",
            config.cache_root().display()
        )
    })?;
    let retention = RetentionConfig::from(&config);
    store
        .enforce_retention(retention.maximum_age, retention.maximum_storage_bytes.get())
        .await
        .context("enforce invocation retention")?;
    let runner = InvocationService::start(store.clone(), RunnerConfig::from(&config)).await?;
    let shutdown_runner = runner.clone();
    let mcp = McpConfig::from(&config);
    let server = BazelMcpServer::new(runner, mcp.result_encoding)
        .with_progress_timing(mcp.progress_initial, mcp.progress_interval)
        .with_task_execution(
            mcp.execution_policy,
            mcp.task_ttl,
            u64::try_from(mcp.task_poll_interval.as_millis()).unwrap_or(u64::MAX),
        );
    tracing::info!(
        policy = ?mcp.execution_policy,
        task_ttl_seconds = mcp.task_ttl.as_secs(),
        task_poll_interval_ms = mcp.task_poll_interval.as_millis(),
        legacy_protocol = "2025-11-25",
        extension_protocol = "2026-06-30",
        "configured negotiated MCP task execution"
    );
    let shutdown_store = store.clone();
    let cleanup_store = store;
    let cleanup_age = retention.maximum_age;
    let cleanup_bytes = retention.maximum_storage_bytes.get();
    let cleanup_interval = retention.cleanup_interval;
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
    let shutdown_wait = config.shutdown_wait();
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
            if tokio::time::timeout(shutdown_wait, shutdown_runner.wait_until_idle())
                .await
                .is_err()
            {
                tracing::warn!(
                    active = shutdown_runner.active_invocation_count().await,
                    "timed out waiting for active Bazel invocations during shutdown"
                );
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
