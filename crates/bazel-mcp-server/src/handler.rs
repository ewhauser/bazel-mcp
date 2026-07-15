use std::{path::PathBuf, str::FromStr};

use bazel_mcp_runner::{InspectRequest, InspectView, InvocationService};
use bazel_mcp_types::{
    BazelCommand, Diagnostic, InvocationId, InvocationRequest, QueryRow, TargetCounts, Termination,
    TestCounts,
};
use rmcp::{
    RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, Meta, ProgressNotificationParam},
    schemars,
    service::Peer,
    tool, tool_handler, tool_router,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::ResultEncoding;

#[derive(Clone)]
pub struct BazelMcpServer {
    runner: InvocationService,
    result_encoding: ResultEncoding,
    progress_initial_delay: std::time::Duration,
    progress_interval: std::time::Duration,
    #[expect(
        dead_code,
        reason = "the rmcp tool_handler macro reads this router field"
    )]
    tool_router: ToolRouter<Self>,
}

impl std::fmt::Debug for BazelMcpServer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BazelMcpServer")
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RunParams {
    /// Absolute path to an allowed Bazel workspace.
    pub workspace: String,
    /// Bazel startup arguments placed before the command.
    #[serde(default)]
    pub startup_args: Vec<String>,
    /// Bazel command such as build, test, coverage, query, cquery, or aquery.
    pub command: String,
    /// Arguments placed after the Bazel command.
    #[serde(default)]
    pub args: Vec<String>,
    /// Optional timeout in seconds.
    pub timeout_seconds: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct InspectParams {
    /// UUID returned by bazel.run.
    pub invocation_id: String,
    /// summary, diagnostics, tests, coverage, artifacts, query_results, or log.
    pub view: String,
    /// Optional literal substring or label glob filter.
    pub filter: Option<String>,
    /// Maximum items, from 1 through 100.
    pub limit: Option<u32>,
    /// Opaque continuation cursor.
    pub cursor: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CancelParams {
    /// UUID returned by bazel.run.
    pub invocation_id: String,
    /// Human-readable reason retained for audit context.
    pub reason: Option<String>,
}

#[derive(Debug, Serialize)]
struct RunResult<'a> {
    invocation_id: String,
    state: bazel_mcp_types::InvocationState,
    command: &'a str,
    exit_code: Option<i32>,
    duration_ms: u64,
    stdout_bytes: u64,
    stderr_bytes: u64,
    headline: &'a str,
    targets: TargetCounts,
    tests: TestCounts,
    diagnostics: &'a [Diagnostic],
    query_result_count: Option<u64>,
    query_sample: Option<&'a [QueryRow]>,
    truncated: bool,
    available_views: &'static [&'static str],
    more_available: bool,
}

impl BazelMcpServer {
    #[must_use]
    pub fn new(runner: InvocationService, result_encoding: ResultEncoding) -> Self {
        Self {
            runner,
            result_encoding,
            progress_initial_delay: std::time::Duration::from_secs(30),
            progress_interval: std::time::Duration::from_secs(60),
            tool_router: Self::tool_router(),
        }
    }

    #[must_use]
    pub fn with_progress_timing(
        mut self,
        initial_delay: std::time::Duration,
        interval: std::time::Duration,
    ) -> Self {
        self.progress_initial_delay = initial_delay;
        self.progress_interval = interval;
        self
    }
}

#[tool_router(router = tool_router)]
impl BazelMcpServer {
    #[tool(
        name = "bazel.run",
        description = "Execute one Bazel command silently and return a bounded actionable summary."
    )]
    async fn bazel_run(
        &self,
        Parameters(params): Parameters<RunParams>,
        cancellation: tokio_util::sync::CancellationToken,
        meta: Meta,
        client: Peer<RoleServer>,
    ) -> Result<CallToolResult, String> {
        let command = match BazelCommand::from_str(&params.command) {
            Ok(command) => command,
            Err(never) => match never {},
        };
        let mut request =
            InvocationRequest::new(PathBuf::from(params.workspace), command, params.args);
        request.startup_arguments = params.startup_args;
        request.timeout_ms = params
            .timeout_seconds
            .map(|seconds| seconds.saturating_mul(1000));
        let id = request.id;
        let runner = self.runner.clone();
        let mut task =
            tokio::spawn(async move { runner.run_with_cancellation(request, cancellation).await });
        let progress_token = meta.get_progress_token();
        let progress_started = std::time::Instant::now();
        let progress_delay = tokio::time::sleep(self.progress_initial_delay);
        tokio::pin!(progress_delay);
        let mut progress_notifications = 0_u64;
        let record = loop {
            tokio::select! {
                result = &mut task => {
                    break result
                        .map_err(|error| error.to_string())?
                        .map_err(|error| error.to_string())?;
                }
                () = &mut progress_delay, if progress_token.is_some() => {
                    if let Some(token) = &progress_token
                        && let Ok(state) = self.runner.invocation_state(id).await
                    {
                        let elapsed_ms = progress_started.elapsed().as_millis() as u64;
                        let message = format!(
                            "state={} elapsed_ms={elapsed_ms}",
                            format!("{state:?}").to_ascii_lowercase(),
                        );
                        if client
                            .notify_progress(
                                ProgressNotificationParam::new(token.clone(), elapsed_ms as f64)
                                    .with_message(message),
                            )
                            .await
                            .is_ok()
                        {
                            progress_notifications = progress_notifications.saturating_add(1);
                        }
                    }
                    progress_delay.as_mut().reset(
                        tokio::time::Instant::now() + self.progress_interval,
                    );
                }
            }
        };
        if progress_notifications > 0 {
            let _ = self
                .runner
                .record_progress_notifications(id, progress_notifications)
                .await;
        }
        let exit_code = match &record.termination {
            Some(Termination::Exit { code }) => Some(*code),
            _ => None,
        };
        let summary = record
            .summary
            .as_ref()
            .ok_or_else(|| "completed invocation has no summary".to_owned())?;
        let mut diagnostic_count = summary.diagnostics.len();
        let mut query_sample_count = summary.query_sample.len();
        loop {
            let result = RunResult {
                invocation_id: record.request.id.to_string(),
                state: record.state,
                command: record.request.command.as_str(),
                exit_code,
                duration_ms: record.metrics.bazel_wall_ms,
                stdout_bytes: record.metrics.raw_stdout_bytes,
                stderr_bytes: record.metrics.raw_stderr_bytes,
                headline: &summary.headline,
                targets: summary.target_counts.clone(),
                tests: summary.test_counts.clone(),
                diagnostics: &summary.diagnostics[..diagnostic_count],
                query_result_count: summary.query_result_count,
                query_sample: (query_sample_count > 0)
                    .then_some(&summary.query_sample[..query_sample_count]),
                truncated: summary.truncated
                    || diagnostic_count < summary.diagnostics.len()
                    || query_sample_count < summary.query_sample.len(),
                available_views: &[
                    "summary",
                    "diagnostics",
                    "tests",
                    "coverage",
                    "artifacts",
                    "query_results",
                    "log",
                ],
                more_available: summary.truncated
                    || diagnostic_count < summary.diagnostics.len()
                    || query_sample_count < summary.query_sample.len()
                    || !summary.targets.is_empty()
                    || !summary.tests.is_empty()
                    || summary.inspect_hint.is_some(),
            };
            let value = serde_json::to_value(&result).map_err(|error| error.to_string())?;
            let limit = if summary.success { 2 * 1024 } else { 8 * 1024 };
            let bytes = self.model_visible_bytes(&value)?;
            if bytes <= limit {
                if let Err(error) = self
                    .runner
                    .record_model_visible_result(record.request.id, bytes, false)
                    .await
                {
                    tracing::warn!(
                        invocation_id = %record.request.id,
                        %error,
                        "could not persist model-visible response metrics"
                    );
                }
                return self.encode(value);
            }
            if query_sample_count > 0 {
                query_sample_count -= 1;
            } else if diagnostic_count > 0 {
                diagnostic_count -= 1;
            } else {
                return Err("bounded bazel.run response could not fit its hard byte limit".into());
            }
        }
    }

    #[tool(
        name = "bazel.inspect",
        description = "Read one bounded, filtered view of a retained Bazel invocation."
    )]
    async fn bazel_inspect(
        &self,
        Parameters(params): Parameters<InspectParams>,
    ) -> Result<CallToolResult, String> {
        let id = parse_id(&params.invocation_id)?;
        let view = match params.view.as_str() {
            "summary" => InspectView::Summary,
            "diagnostics" => InspectView::Diagnostics,
            "tests" => InspectView::Tests,
            "log" => InspectView::Log,
            "coverage" => InspectView::Coverage,
            "artifacts" => InspectView::Artifacts,
            "query_results" => InspectView::QueryResults,
            other => return Err(format!("unsupported inspection view: {other}")),
        };
        let result = self
            .runner
            .inspect(InspectRequest {
                invocation_id: Some(id),
                workspace: None,
                view,
                cursor: params.cursor,
                filter: params.filter,
                limit: params.limit.unwrap_or(20).clamp(1, 100),
                max_bytes: self.single_representation_budget(8 * 1024),
            })
            .await
            .map_err(|error| error.to_string())?;
        let value = serde_json::to_value(result).map_err(|error| error.to_string())?;
        let bytes = self.model_visible_bytes(&value)?;
        if let Err(error) = self
            .runner
            .record_model_visible_result(id, bytes, true)
            .await
        {
            tracing::warn!(
                invocation_id = %id,
                %error,
                "could not persist model-visible inspection metrics"
            );
        }
        self.encode(value)
    }

    #[tool(
        name = "bazel.cancel",
        description = "Cancel one queued or running Bazel invocation by UUID."
    )]
    async fn bazel_cancel(
        &self,
        Parameters(params): Parameters<CancelParams>,
    ) -> Result<CallToolResult, String> {
        let id = parse_id(&params.invocation_id)?;
        let result = self
            .runner
            .cancel_with_reason(id, params.reason.as_deref())
            .await
            .map_err(|error| error.to_string())?;
        self.encode(serde_json::to_value(result).map_err(|error| error.to_string())?)
    }
}

impl BazelMcpServer {
    fn single_representation_budget(&self, visible_budget: usize) -> usize {
        match self.result_encoding {
            ResultEncoding::Text | ResultEncoding::Structured => visible_budget,
            ResultEncoding::Both => visible_budget / 2,
        }
    }

    fn model_visible_bytes(&self, value: &serde_json::Value) -> Result<usize, String> {
        let bytes = serde_json::to_vec(value)
            .map_err(|error| error.to_string())?
            .len();
        Ok(match self.result_encoding {
            ResultEncoding::Text | ResultEncoding::Structured => bytes,
            ResultEncoding::Both => bytes.saturating_mul(2),
        })
    }

    fn encode(&self, value: serde_json::Value) -> Result<CallToolResult, String> {
        Ok(match self.result_encoding {
            ResultEncoding::Text => {
                CallToolResult::success(vec![ContentBlock::text(value.to_string())])
            }
            ResultEncoding::Structured => {
                let mut result = CallToolResult::default();
                result.structured_content = Some(value);
                result.is_error = Some(false);
                result
            }
            ResultEncoding::Both => CallToolResult::structured(value),
        })
    }
}

#[tool_handler(
    name = "bazel-mcp",
    version = "0.1.0",
    instructions = "Run Bazel through bazel.run; use bazel.inspect only for bounded follow-up evidence."
)]
impl ServerHandler for BazelMcpServer {}

fn parse_id(value: &str) -> Result<InvocationId, String> {
    Uuid::parse_str(value)
        .map(InvocationId::from_uuid)
        .map_err(|_| "invocation_id must be a UUID".to_owned())
}

#[cfg(test)]
mod tests {
    use bazel_mcp_runner::RunnerConfig;
    use bazel_mcp_store::Store;
    use tempfile::tempdir;

    use super::*;

    #[tokio::test]
    async fn both_encoding_charges_and_budgets_both_visible_representations() {
        let root = tempdir().unwrap();
        let runner = InvocationService::new(
            Store::open(root.path()).await.unwrap(),
            RunnerConfig::default(),
        )
        .unwrap();
        let value = serde_json::json!({"message": "hello"});
        let one = serde_json::to_vec(&value).unwrap().len();

        let text = BazelMcpServer::new(runner.clone(), ResultEncoding::Text);
        assert_eq!(text.model_visible_bytes(&value).unwrap(), one);
        assert_eq!(text.single_representation_budget(8_192), 8_192);

        let both = BazelMcpServer::new(runner, ResultEncoding::Both);
        assert_eq!(both.model_visible_bytes(&value).unwrap(), one * 2);
        assert_eq!(both.single_representation_budget(8_192), 4_096);
    }
}
