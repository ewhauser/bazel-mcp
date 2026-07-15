use clap::Parser;
use tracing_subscriber::EnvFilter;

use bazel_mcp_server::{Cli, ServerConfig};

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(&cli.log))
        .with_writer(std::io::stderr)
        .init();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!(%error, "could not start async runtime");
            return std::process::ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(run(cli));
    // Tokio's standard-input adapter may own a blocking read that cannot be
    // interrupted by SIGTERM while an MCP client keeps the pipe open. Bound
    // runtime teardown after the service has cancelled and joined Bazel work.
    runtime.shutdown_timeout(std::time::Duration::from_secs(2));
    result
}

async fn run(cli: Cli) -> std::process::ExitCode {
    match ServerConfig::load(&cli) {
        Ok(config) => match bazel_mcp_server::serve(config).await {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(error) => {
                tracing::error!(%error, "bazel-mcp failed");
                std::process::ExitCode::FAILURE
            }
        },
        Err(error) => {
            tracing::error!(%error, "invalid configuration");
            std::process::ExitCode::FAILURE
        }
    }
}
