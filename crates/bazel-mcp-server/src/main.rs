use clap::Parser;
use tracing_subscriber::EnvFilter;

use bazel_mcp_server::{
    Cli, LaunchMode, ValidatedServerConfig, agent_log_filter, detect_launch, run_agent,
};

fn main() -> std::process::ExitCode {
    let launch = detect_launch(std::env::args_os());
    match launch {
        LaunchMode::Mcp => run_mcp(Cli::parse()),
        LaunchMode::Agent(arguments) => run_agent_mode(arguments),
    }
}

fn run_mcp(cli: Cli) -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(&cli.log))
        .with_writer(std::io::stderr)
        .init();
    run_runtime(run_server(cli))
}

fn run_agent_mode(arguments: Vec<std::ffi::OsString>) -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(agent_log_filter()))
        .with_writer(std::io::stderr)
        .init();
    run_runtime(run_agent(arguments))
}

fn run_runtime<F>(future: F) -> std::process::ExitCode
where
    F: std::future::Future<Output = std::process::ExitCode>,
{
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
    let result = runtime.block_on(future);
    // Tokio's standard-input adapter may own a blocking read that cannot be
    // interrupted by SIGTERM while an MCP client keeps the pipe open. Bound
    // runtime teardown after the service has cancelled and joined Bazel work.
    runtime.shutdown_timeout(std::time::Duration::from_secs(2));
    result
}

async fn run_server(cli: Cli) -> std::process::ExitCode {
    match ValidatedServerConfig::load(&cli) {
        Ok(config) => match bazel_mcp_server::serve(config).await {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(error) => {
                tracing::error!(error = %format_args!("{error:#}"), "bazel-mcp failed");
                std::process::ExitCode::FAILURE
            }
        },
        Err(error) => {
            tracing::error!(error = %format_args!("{error:#}"), "invalid configuration");
            std::process::ExitCode::FAILURE
        }
    }
}
