use clap::Parser;
use tracing_subscriber::EnvFilter;

use bazel_mcp_server::{Cli, ServerConfig};

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(&cli.log))
        .with_writer(std::io::stderr)
        .init();
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
