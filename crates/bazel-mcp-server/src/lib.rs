//! Thin stdio MCP boundary for the Bazel invocation service.

mod config;
mod handler;

pub use config::{Cli, ResultEncoding, ServerConfig};
pub use handler::BazelMcpServer;

use anyhow::Context;
use bazel_mcp_policy::PolicyConfig;
use bazel_mcp_runner::{InvocationService, RunnerConfig};
use bazel_mcp_store::Store;
use rmcp::ServiceExt;

pub async fn serve(config: ServerConfig) -> anyhow::Result<()> {
    let store = Store::open(&config.cache_root)
        .await
        .context("open invocation store")?;
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
    let runner = InvocationService::new(
        store,
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
        },
    )?;
    let server = BazelMcpServer::new(runner, config.result_encoding).with_progress_timing(
        std::time::Duration::from_secs(config.progress_initial_seconds),
        std::time::Duration::from_secs(config.progress_interval_seconds),
    );
    server
        .serve(rmcp::transport::stdio())
        .await
        .context("start stdio MCP transport")?
        .waiting()
        .await
        .context("run stdio MCP server")?;
    Ok(())
}
