use std::{path::PathBuf, str::FromStr, time::Duration};

use bazel_mcp_runner::{InspectRequest, InspectView, InvocationService};
use bazel_mcp_types::{
    BazelCommand, DeferredResultView, DeferredRetrieval, DeferredTerminalState, InvocationId,
    InvocationRecord, InvocationRequest, InvocationState, PageRequest, ResultDisposition,
};
use rmcp::{
    ErrorData, RoleServer,
    handler::server::{router::tool::ToolRouter, tool::ToolCallContext, wrapper::Parameters},
    model::*,
    schemars,
    service::{NotificationContext, Peer, RequestContext, Service, ServiceRole},
    tool, tool_router,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    McpExecutionPolicy, ResultEncoding,
    protocol::{
        IMMEDIATE_RESPONSE, LegacyTasksAdapter, ProtocolAdapter, ProtocolFamily, TASKS_EXTENSION,
        TasksExtensionAdapter,
    },
    result::{EncodedResult, ExecutionResult, ResultEncoder, RunResultBuilder},
    tasks::{TaskResultState, TaskSnapshot},
};

#[derive(Clone)]
pub struct BazelMcpServer {
    runner: InvocationService,
    result_encoder: ResultEncoder,
    run_result_builder: RunResultBuilder,
    progress_initial_delay: std::time::Duration,
    progress_interval: std::time::Duration,
    execution_policy: McpExecutionPolicy,
    task_ttl: Duration,
    task_poll_interval_ms: u64,
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
    /// Bazel command or a configured Aspect command such as lint.
    pub command: String,
    /// Arguments placed after the selected command.
    #[serde(default)]
    pub args: Vec<String>,
    /// Optional timeout in seconds.
    pub timeout_seconds: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct InspectParams {
    /// UUID returned by bazel.run.
    pub invocation_id: Option<String>,
    /// Optional absolute workspace path used to scope retained invocation listings.
    pub workspace: Option<String>,
    /// Optional invocation state used to filter retained invocation listings.
    pub state: Option<String>,
    /// Optional Bazel command used to filter retained invocation listings.
    pub command: Option<String>,
    /// summary, metrics, diagnostics, tests, coverage, artifacts, query_results, log, test_log, or invocations.
    pub view: String,
    /// Optional literal substring or label glob filter.
    pub filter: Option<String>,
    /// Maximum items, from 1 through 100.
    pub limit: Option<u32>,
    /// Maximum model-visible response bytes, clamped from 512 through 8192.
    pub max_bytes: Option<u32>,
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

impl BazelMcpServer {
    #[must_use]
    pub fn new(runner: InvocationService, result_encoding: ResultEncoding) -> Self {
        let result_encoder = ResultEncoder::new(result_encoding);
        Self {
            runner,
            result_encoder: result_encoder.clone(),
            run_result_builder: RunResultBuilder::new(result_encoder),
            progress_initial_delay: std::time::Duration::from_secs(30),
            progress_interval: std::time::Duration::from_secs(60),
            execution_policy: McpExecutionPolicy::Auto,
            task_ttl: Duration::from_secs(24 * 60 * 60),
            task_poll_interval_ms: 2_000,
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

    #[must_use]
    pub fn with_task_execution(
        mut self,
        policy: McpExecutionPolicy,
        ttl: Duration,
        poll_interval_ms: u64,
    ) -> Self {
        self.execution_policy = policy;
        self.task_ttl = ttl;
        self.task_poll_interval_ms = poll_interval_ms;
        self
    }
}

#[tool_router(router = tool_router)]
impl BazelMcpServer {
    #[tool(
        name = "bazel.run",
        description = "Execute one allowed Bazel or configured Aspect command silently and return a bounded actionable summary."
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
                        && let Ok(progress) = self.runner.invocation_progress(id).await
                    {
                        let elapsed_ms = progress_started.elapsed().as_millis() as u64;
                        let mut message = format!(
                            "state={} elapsed_ms={elapsed_ms}",
                            format!("{:?}", progress.state).to_ascii_lowercase(),
                        );
                        if let Some(phase) = progress.phase {
                            message.push_str(&format!(
                                " phase={phase} output_base_lock_wait_ms={}",
                                progress.output_base_lock_wait_ms,
                            ));
                            if let Some(owner) = progress.output_base_lock_owner {
                                message.push_str(&format!(" owner={owner}"));
                            }
                        }
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
        self.build_run_result(&record, false).await
    }

    #[tool(
        name = "bazel.inspect",
        description = "Read bounded retained invocation evidence or list recent invocations."
    )]
    async fn bazel_inspect(
        &self,
        Parameters(params): Parameters<InspectParams>,
    ) -> Result<CallToolResult, String> {
        let view = InspectView::parse(&params.view)
            .ok_or_else(|| format!("unsupported inspection view: {}", params.view))?;
        let id = if view == InspectView::Invocations {
            if params.invocation_id.is_some() {
                return Err("invocation_id is not supported for the invocations view".into());
            }
            None
        } else {
            let value = params
                .invocation_id
                .as_deref()
                .ok_or_else(|| "invocation_id is required for this view".to_owned())?;
            Some(parse_id(value)?)
        };
        let workspace = params.workspace.map(PathBuf::from);
        if workspace.is_some() && view != InspectView::Invocations {
            return Err("workspace is supported only for the invocations view".into());
        }
        let state = params
            .state
            .as_deref()
            .map(parse_inspect_state)
            .transpose()?;
        if state.is_some() && view != InspectView::Invocations {
            return Err("state is supported only for the invocations view".into());
        }
        let command = params
            .command
            .as_deref()
            .map(|value| match BazelCommand::from_str(value) {
                Ok(command) => command,
                Err(never) => match never {},
            });
        if command.is_some() && view != InspectView::Invocations {
            return Err("command is supported only for the invocations view".into());
        }
        let visible_budget = inspect_visible_budget(params.max_bytes);
        let item_limit = params.limit.unwrap_or(20).clamp(1, 100);
        let result = self
            .runner
            .inspect(InspectRequest {
                invocation_id: id,
                workspace,
                state,
                command,
                view,
                cursor: params.cursor,
                filter: params.filter,
                item_limit,
                scan_limit: item_limit.saturating_mul(20).clamp(1_000, 10_000),
            })
            .await
            .map_err(|error| error.to_string())?;
        let encoded = self.result_encoder.encode_inspect(result, visible_budget)?;
        if let Some(id) = id
            && let Err(error) = self
                .runner
                .record_model_visible_result(id, encoded.visible_bytes, true)
                .await
        {
            tracing::warn!(
                invocation_id = %id,
                %error,
                "could not persist model-visible inspection metrics"
            );
        }
        Ok(encoded.result)
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
        self.result_encoder
            .encode(&result)
            .map(|encoded| encoded.result)
    }
}

impl BazelMcpServer {
    async fn build_run_result(
        &self,
        record: &InvocationRecord,
        tool_error: bool,
    ) -> Result<CallToolResult, String> {
        let EncodedResult {
            result,
            visible_bytes,
        } = self
            .run_result_builder
            .build(ExecutionResult::new(record, tool_error))?;
        if let Err(error) = self
            .runner
            .record_model_visible_result(record.request.id, visible_bytes, false)
            .await
        {
            tracing::warn!(
                invocation_id = %record.request.id,
                %error,
                "could not persist model-visible response metrics"
            );
        }
        Ok(result)
    }

    #[cfg(test)]
    fn model_visible_bytes(&self, value: &serde_json::Value) -> Result<usize, String> {
        self.result_encoder
            .encode_value(value.clone())
            .map(|encoded| encoded.visible_bytes)
    }

    #[cfg(test)]
    fn encode(&self, value: serde_json::Value) -> Result<CallToolResult, String> {
        self.result_encoder
            .encode_value(value)
            .map(|encoded| encoded.result)
    }

    #[cfg(test)]
    fn encode_toon(value: &serde_json::Value) -> Result<String, String> {
        toon_format::encode_default(value).map_err(|error| format!("encode TOON result: {error}"))
    }
}

impl BazelMcpServer {
    fn protocol_family(version: Option<&ProtocolVersion>) -> ProtocolFamily {
        ProtocolAdapter::negotiate(version).family()
    }

    fn initialize_result(&self, version: ProtocolVersion) -> InitializeResult {
        let adapter = ProtocolAdapter::negotiate(Some(&version));
        let capabilities = adapter.capabilities(self.execution_policy);
        let mut result = InitializeResult::new(capabilities)
            .with_server_info(Implementation::new("bazel-mcp", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Run Bazel through bazel.run; use bazel.inspect only for bounded follow-up evidence.",
            );
        result.protocol_version = version;
        result
    }

    fn tools_for(&self, family: ProtocolFamily) -> Vec<Tool> {
        let mut tools = self.tool_router.list_all();
        let adapter = match family {
            ProtocolFamily::Earlier => ProtocolAdapter::negotiate(None),
            ProtocolFamily::LegacyTasks => {
                ProtocolAdapter::negotiate(Some(&ProtocolVersion::V_2025_11_25))
            }
            ProtocolFamily::TasksExtension => ProtocolAdapter::Extension(TasksExtensionAdapter),
        };
        adapter.adapt_tools(&mut tools, self.execution_policy);
        tools
    }

    fn extension_capable(meta: &Meta) -> bool {
        meta.client_capabilities()
            .and_then(|capabilities| capabilities.extensions)
            .is_some_and(|extensions| extensions.contains_key(TASKS_EXTENSION))
    }

    fn trace_task_call(family: ProtocolFamily, method: &'static str) {
        tracing::info!(
            target: "bazel_mcp::metrics",
            metric = "task_protocol_calls_total",
            increment = 1_u64,
            protocol_family = family.as_str(),
            method,
            "handled task protocol call"
        );
    }

    fn trace_task_response<T: Serialize>(
        family: ProtocolFamily,
        phase: &'static str,
        response: &T,
    ) {
        if let Ok(response) = serde_json::to_vec(response) {
            tracing::info!(
                target: "bazel_mcp::metrics",
                metric = "task_protocol_response_bytes",
                protocol_family = family.as_str(),
                phase,
                observe_bytes = response.len(),
                "observed task protocol response size"
            );
        }
    }

    fn missing_extension_capability() -> ErrorData {
        tracing::info!(
            target: "bazel_mcp::metrics",
            metric = "task_capability_mismatch_total",
            increment = 1_u64,
            protocol_family = ProtocolFamily::TasksExtension.as_str(),
            "rejected request missing the task extension capability"
        );
        ErrorData::new(
            ErrorCode(-32_003),
            "Missing Required Client Capability",
            Some(serde_json::json!({
                "requiredCapabilities": {
                    "extensions": { (TASKS_EXTENSION): {} }
                }
            })),
        )
    }

    fn method_not_found(message: impl Into<String>) -> ErrorData {
        ErrorData::new(ErrorCode::METHOD_NOT_FOUND, message.into(), None)
    }

    fn task_not_found(id: &str) -> ErrorData {
        tracing::info!(
            target: "bazel_mcp::metrics",
            metric = "task_unknown_or_expired_id_total",
            increment = 1_u64,
            "rejected unknown or expired deferred result identifier"
        );
        ErrorData::invalid_params(format!("unknown or expired task ID: {id}"), None)
    }

    fn decode_run_request(arguments: Option<JsonObject>) -> Result<InvocationRequest, String> {
        let params: RunParams =
            serde_json::from_value(serde_json::Value::Object(arguments.unwrap_or_default()))
                .map_err(|error| format!("invalid bazel.run arguments: {error}"))?;
        let command = match BazelCommand::from_str(&params.command) {
            Ok(command) => command,
            Err(never) => match never {},
        };
        let mut request =
            InvocationRequest::new(PathBuf::from(params.workspace), command, params.args);
        request.startup_arguments = params.startup_args;
        request.timeout_ms = params
            .timeout_seconds
            .map(|seconds| seconds.saturating_mul(1_000));
        Ok(request)
    }

    async fn submit_deferred(
        &self,
        arguments: Option<JsonObject>,
        retrieval: DeferredRetrieval,
    ) -> Result<DeferredResultView, CallToolResult> {
        let started = std::time::Instant::now();
        let request = Self::decode_run_request(arguments).map_err(tool_error)?;
        let ttl_ms = i64::try_from(self.task_ttl.as_millis()).unwrap_or(i64::MAX);
        let expires_at_ms = bazel_mcp_types::unix_timestamp_ms().saturating_add(ttl_ms);
        let id = self
            .runner
            .submit(
                request,
                ResultDisposition::Deferred {
                    retrieval,
                    expires_at_ms,
                },
            )
            .await
            .map_err(|error| tool_error(error.to_string()))?;
        let mut view = self
            .runner
            .deferred_result(id, retrieval)
            .await
            .map_err(|error| tool_error(error.to_string()))?;
        let minimum_expiry = view.deferred.created_at_ms.saturating_add(ttl_ms);
        if view.deferred.expires_at_ms < minimum_expiry {
            self.runner
                .extend_deferred_expiry(id, minimum_expiry)
                .await
                .map_err(|error| tool_error(error.to_string()))?;
            view = self
                .runner
                .deferred_result(id, retrieval)
                .await
                .map_err(|error| tool_error(error.to_string()))?;
        }
        tracing::info!(
            target: "bazel_mcp::metrics",
            metric = "deferred_invocations_accepted_total",
            increment = 1_u64,
            retrieval = retrieval.as_str(),
            "deferred invocation accepted"
        );
        tracing::info!(
            target: "bazel_mcp::metrics",
            metric = "task_creation_acknowledgement_latency_ms",
            observe_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            retrieval = retrieval.as_str(),
            "observed task creation acknowledgement latency"
        );
        Ok(view)
    }

    fn legacy_task(&self, view: &DeferredResultView) -> Task {
        let snapshot = TaskSnapshot::from_view(view, self.task_poll_interval_ms);
        LegacyTasksAdapter.task(&snapshot)
    }

    fn extension_task_value(&self, view: &DeferredResultView) -> serde_json::Value {
        let snapshot = TaskSnapshot::from_view(view, self.task_poll_interval_ms);
        TasksExtensionAdapter.creation(&snapshot)
    }

    async fn extension_get_value(
        &self,
        view: &DeferredResultView,
    ) -> Result<serde_json::Value, ErrorData> {
        let snapshot = TaskSnapshot::from_view(view, self.task_poll_interval_ms);
        let (result, error) = match &snapshot.result {
            TaskResultState::Invocation { tool_error } if snapshot.state.is_terminal() => {
                let result = self
                    .build_run_result(&view.invocation, *tool_error)
                    .await
                    .map_err(|error| ErrorData::internal_error(error, None))?;
                (Some(result), None)
            }
            TaskResultState::InternalFailure { redacted_message } => {
                (None, Some(redacted_message.as_str()))
            }
            TaskResultState::Pending | TaskResultState::Cancelled => (None, None),
            TaskResultState::Invocation { .. } => (None, None),
        };
        TasksExtensionAdapter
            .complete(&snapshot, result, error)
            .map_err(|error| ErrorData::internal_error(error.to_string(), None))
    }

    async fn call_tool_request(
        &self,
        params: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ServerResult, ErrorData> {
        let family = Self::protocol_family(context.protocol_version().as_ref());
        let is_run = params.name == "bazel.run";
        match family {
            ProtocolFamily::LegacyTasks => {
                if self.execution_policy != McpExecutionPolicy::SyncOnly && params.task.is_some() {
                    if !is_run {
                        return Err(Self::method_not_found(
                            "task execution is not supported for this tool",
                        ));
                    }
                    Self::trace_task_call(ProtocolFamily::LegacyTasks, "tools/call");
                    let view = match self
                        .submit_deferred(params.arguments, DeferredRetrieval::SeparateResult)
                        .await
                    {
                        Ok(view) => view,
                        Err(result) => return Ok(ServerResult::CallToolResult(result)),
                    };
                    let task = self.legacy_task(&view);
                    let mut meta = Meta::new();
                    meta.0.insert(
                        IMMEDIATE_RESPONSE.to_owned(),
                        serde_json::Value::String(format!(
                            "Bazel invocation {} is running.",
                            view.deferred.invocation_id
                        )),
                    );
                    meta.0.insert(
                        RelatedTaskMetadata::META_KEY.to_owned(),
                        serde_json::json!({"taskId": view.deferred.invocation_id.to_string()}),
                    );
                    let result = CreateTaskResult::new(task).with_meta(meta);
                    Self::trace_task_response(ProtocolFamily::LegacyTasks, "creation", &result);
                    return Ok(ServerResult::CreateTaskResult(result));
                }
                if is_run
                    && self.execution_policy == McpExecutionPolicy::TasksRequired
                    && params.task.is_none()
                {
                    tracing::info!(
                        target: "bazel_mcp::metrics",
                        metric = "task_capability_mismatch_total",
                        increment = 1_u64,
                        protocol_family = ProtocolFamily::LegacyTasks.as_str(),
                        "rejected request missing legacy task augmentation"
                    );
                    return Err(Self::method_not_found(
                        "bazel.run requires task-based invocation",
                    ));
                }
            }
            ProtocolFamily::TasksExtension => {
                if is_run && self.execution_policy != McpExecutionPolicy::SyncOnly {
                    if !Self::extension_capable(&context.meta) {
                        if self.execution_policy == McpExecutionPolicy::TasksRequired {
                            return Err(Self::missing_extension_capability());
                        }
                    } else {
                        Self::trace_task_call(ProtocolFamily::TasksExtension, "tools/call");
                        let view = match self
                            .submit_deferred(params.arguments, DeferredRetrieval::InlineResult)
                            .await
                        {
                            Ok(view) => view,
                            Err(result) => return Ok(ServerResult::CallToolResult(result)),
                        };
                        let result = CustomResult::new(self.extension_task_value(&view));
                        Self::trace_task_response(
                            ProtocolFamily::TasksExtension,
                            "creation",
                            &result,
                        );
                        return Ok(ServerResult::CustomResult(result));
                    }
                }
            }
            ProtocolFamily::Earlier => {
                if is_run && self.execution_policy == McpExecutionPolicy::TasksRequired {
                    tracing::info!(
                        target: "bazel_mcp::metrics",
                        metric = "task_capability_mismatch_total",
                        increment = 1_u64,
                        protocol_family = ProtocolFamily::Earlier.as_str(),
                        "rejected request on a protocol without task support"
                    );
                    return Err(Self::method_not_found(
                        "task execution is unavailable for the negotiated protocol",
                    ));
                }
            }
        }
        self.tool_router
            .call(ToolCallContext::new(self, params, context))
            .await
            .map(ServerResult::CallToolResult)
    }

    async fn legacy_get_task(&self, task_id: &str) -> Result<GetTaskResult, ErrorData> {
        let id = parse_task_id(task_id)?;
        let view = self
            .runner
            .deferred_result(id, DeferredRetrieval::SeparateResult)
            .await
            .map_err(|_| Self::task_not_found(task_id))?;
        let result = GetTaskResult::new(self.legacy_task(&view));
        Self::trace_task_response(ProtocolFamily::LegacyTasks, "polling", &result);
        Ok(result)
    }

    async fn legacy_task_result(
        &self,
        task_id: &str,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<GetTaskPayloadResult, ErrorData> {
        let id = parse_task_id(task_id)?;
        self.runner
            .deferred_result(id, DeferredRetrieval::SeparateResult)
            .await
            .map_err(|_| Self::task_not_found(task_id))?;
        let record = self
            .runner
            .wait(id, cancellation)
            .await
            .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
        let view = self
            .runner
            .deferred_result(id, DeferredRetrieval::SeparateResult)
            .await
            .map_err(|_| Self::task_not_found(task_id))?;
        let mut result = self
            .build_run_result(&record, view.deferred.failure.is_some())
            .await
            .map_err(|error| ErrorData::internal_error(error, None))?;
        let mut meta = result.meta.take().unwrap_or_default();
        meta.0.insert(
            RelatedTaskMetadata::META_KEY.to_owned(),
            serde_json::json!({"taskId": task_id}),
        );
        result.meta = Some(meta);
        let result = serde_json::to_value(result)
            .map(GetTaskPayloadResult::new)
            .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
        Self::trace_task_response(ProtocolFamily::LegacyTasks, "final_result", &result);
        Ok(result)
    }

    async fn legacy_cancel_task(
        &self,
        task_id: &str,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<CancelTaskResult, ErrorData> {
        let id = parse_task_id(task_id)?;
        let view = self
            .runner
            .deferred_result(id, DeferredRetrieval::SeparateResult)
            .await
            .map_err(|_| Self::task_not_found(task_id))?;
        if view.invocation.state.is_terminal()
            || view.deferred.terminal_override == Some(DeferredTerminalState::Cancelled)
        {
            return Err(ErrorData::invalid_params("task is already terminal", None));
        }
        self.runner
            .record_deferred_cancellation(id)
            .await
            .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
        self.runner
            .cancel(id)
            .await
            .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
        let finalizer = self.runner.clone();
        tokio::spawn(async move {
            if finalizer
                .wait(id, tokio_util::sync::CancellationToken::new())
                .await
                .is_ok()
            {
                let _ = finalizer.set_deferred_cancelled(id).await;
            }
        });
        self.runner
            .wait(id, cancellation)
            .await
            .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
        self.runner
            .set_deferred_cancelled(id)
            .await
            .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
        let view = self
            .runner
            .deferred_result(id, DeferredRetrieval::SeparateResult)
            .await
            .map_err(|_| Self::task_not_found(task_id))?;
        let result = CancelTaskResult::new(self.legacy_task(&view));
        Self::trace_task_response(ProtocolFamily::LegacyTasks, "control", &result);
        Ok(result)
    }

    async fn extension_get_task(&self, task_id: &str) -> Result<CustomResult, ErrorData> {
        let id = parse_task_id(task_id)?;
        let view = self
            .runner
            .deferred_result(id, DeferredRetrieval::InlineResult)
            .await
            .map_err(|_| Self::task_not_found(task_id))?;
        let terminal = TaskSnapshot::from_view(&view, self.task_poll_interval_ms)
            .state
            .is_terminal();
        let result = self
            .extension_get_value(&view)
            .await
            .map(CustomResult::new)?;
        Self::trace_task_response(
            ProtocolFamily::TasksExtension,
            if terminal { "final_result" } else { "polling" },
            &result,
        );
        Ok(result)
    }

    async fn extension_cancel_task(&self, task_id: &str) -> Result<CustomResult, ErrorData> {
        let id = parse_task_id(task_id)?;
        self.runner
            .deferred_result(id, DeferredRetrieval::InlineResult)
            .await
            .map_err(|_| Self::task_not_found(task_id))?;
        self.runner
            .record_deferred_cancellation(id)
            .await
            .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
        self.runner
            .cancel(id)
            .await
            .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
        let result = TasksExtensionAdapter.acknowledge();
        Self::trace_task_response(ProtocolFamily::TasksExtension, "control", &result);
        Ok(result)
    }

    async fn custom_request(
        &self,
        request: CustomRequest,
        context: RequestContext<RoleServer>,
    ) -> Result<ServerResult, ErrorData> {
        match request.method.as_str() {
            "server/discover" => Ok(ServerResult::CustomResult(CustomResult::new(
                serde_json::json!({
                    "capabilities": {
                        "extensions": { (TASKS_EXTENSION): {} }
                    }
                }),
            ))),
            "tasks/update"
                if Self::protocol_family(context.protocol_version().as_ref())
                    == ProtocolFamily::TasksExtension =>
            {
                Self::trace_task_call(ProtocolFamily::TasksExtension, "tasks/update");
                if !Self::extension_capable(&context.meta) {
                    return Err(Self::missing_extension_capability());
                }
                let task_id = request
                    .params
                    .as_ref()
                    .and_then(|params| params.get("taskId"))
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| ErrorData::invalid_params("taskId is required", None))?;
                let id = parse_task_id(task_id)?;
                self.runner
                    .deferred_result(id, DeferredRetrieval::InlineResult)
                    .await
                    .map_err(|_| Self::task_not_found(task_id))?;
                let result = TasksExtensionAdapter.acknowledge();
                Self::trace_task_response(ProtocolFamily::TasksExtension, "control", &result);
                Ok(ServerResult::CustomResult(result))
            }
            _ => Err(Self::method_not_found(request.method)),
        }
    }
}

impl Service<RoleServer> for BazelMcpServer {
    async fn handle_request(
        &self,
        request: <RoleServer as ServiceRole>::PeerReq,
        context: RequestContext<RoleServer>,
    ) -> Result<<RoleServer as ServiceRole>::Resp, ErrorData> {
        match request {
            ClientRequest::InitializeRequest(request) => {
                let requested = request.params.protocol_version.clone();
                let negotiated = if ProtocolVersion::KNOWN_VERSIONS.contains(&requested)
                    || requested.as_str() == "2026-06-30"
                {
                    requested
                } else {
                    ProtocolVersion::LATEST
                };
                let mut peer_info = request.params;
                peer_info.protocol_version = negotiated.clone();
                context.peer.set_peer_info(peer_info);
                tracing::info!(
                    protocol_family = Self::protocol_family(Some(&negotiated)).as_str(),
                    protocol_version = negotiated.as_str(),
                    "negotiated MCP protocol family"
                );
                Ok(ServerResult::InitializeResult(
                    self.initialize_result(negotiated),
                ))
            }
            ClientRequest::PingRequest(_) => Ok(ServerResult::empty(())),
            ClientRequest::ListToolsRequest(_) => {
                let family = Self::protocol_family(context.protocol_version().as_ref());
                Ok(ServerResult::ListToolsResult(
                    ListToolsResult::with_all_items(self.tools_for(family)),
                ))
            }
            ClientRequest::CallToolRequest(request) => {
                self.call_tool_request(request.params, context).await
            }
            ClientRequest::GetTaskRequest(request) => {
                let family = Self::protocol_family(context.protocol_version().as_ref());
                Self::trace_task_call(family, "tasks/get");
                match family {
                    ProtocolFamily::LegacyTasks => self
                        .legacy_get_task(&request.params.task_id)
                        .await
                        .map(ServerResult::GetTaskResult),
                    ProtocolFamily::TasksExtension => {
                        if !Self::extension_capable(&context.meta) {
                            return Err(Self::missing_extension_capability());
                        }
                        self.extension_get_task(&request.params.task_id)
                            .await
                            .map(ServerResult::CustomResult)
                    }
                    ProtocolFamily::Earlier => Err(Self::method_not_found("tasks/get")),
                }
            }
            ClientRequest::ListTasksRequest(request) => {
                let family = Self::protocol_family(context.protocol_version().as_ref());
                Self::trace_task_call(family, "tasks/list");
                if family != ProtocolFamily::LegacyTasks {
                    return Err(Self::method_not_found("tasks/list"));
                }
                let cursor = request.params.and_then(|params| params.cursor);
                let page = self
                    .runner
                    .list_deferred_results(
                        DeferredRetrieval::SeparateResult,
                        PageRequest::new(cursor, 100),
                    )
                    .await
                    .map_err(|error| ErrorData::invalid_params(error.to_string(), None))?;
                let mut result = ListTasksResult::new(
                    page.items
                        .iter()
                        .map(|view| self.legacy_task(view))
                        .collect(),
                );
                result.next_cursor = page.next_cursor;
                Self::trace_task_response(ProtocolFamily::LegacyTasks, "polling", &result);
                Ok(ServerResult::ListTasksResult(result))
            }
            ClientRequest::GetTaskPayloadRequest(request) => {
                let family = Self::protocol_family(context.protocol_version().as_ref());
                Self::trace_task_call(family, "tasks/result");
                if family != ProtocolFamily::LegacyTasks {
                    return Err(Self::method_not_found("tasks/result"));
                }
                self.legacy_task_result(&request.params.task_id, context.ct)
                    .await
                    .map(ServerResult::GetTaskPayloadResult)
            }
            ClientRequest::CancelTaskRequest(request) => {
                let family = Self::protocol_family(context.protocol_version().as_ref());
                Self::trace_task_call(family, "tasks/cancel");
                match family {
                    ProtocolFamily::LegacyTasks => self
                        .legacy_cancel_task(&request.params.task_id, context.ct)
                        .await
                        .map(ServerResult::CancelTaskResult),
                    ProtocolFamily::TasksExtension => {
                        if !Self::extension_capable(&context.meta) {
                            return Err(Self::missing_extension_capability());
                        }
                        self.extension_cancel_task(&request.params.task_id)
                            .await
                            .map(ServerResult::CustomResult)
                    }
                    ProtocolFamily::Earlier => Err(Self::method_not_found("tasks/cancel")),
                }
            }
            ClientRequest::CustomRequest(request) => self.custom_request(request, context).await,
            other => Err(Self::method_not_found(other.method())),
        }
    }

    async fn handle_notification(
        &self,
        _notification: <RoleServer as ServiceRole>::PeerNot,
        _context: NotificationContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        Ok(())
    }

    fn get_info(&self) -> <RoleServer as ServiceRole>::Info {
        self.initialize_result(ProtocolVersion::LATEST)
    }
}

fn tool_error(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message.into())])
}

fn parse_task_id(value: &str) -> Result<InvocationId, ErrorData> {
    parse_id(value).map_err(|_| ErrorData::invalid_params("taskId must be a UUID", None))
}

fn parse_id(value: &str) -> Result<InvocationId, String> {
    Uuid::parse_str(value)
        .map(InvocationId::from_uuid)
        .map_err(|_| "invocation_id must be a UUID".to_owned())
}

fn parse_inspect_state(value: &str) -> Result<InvocationState, String> {
    match value {
        "queued" => Ok(InvocationState::Queued),
        "starting" => Ok(InvocationState::Starting),
        "running" => Ok(InvocationState::Running),
        "succeeded" => Ok(InvocationState::Succeeded),
        "failed" => Ok(InvocationState::Failed),
        "cancelled" => Ok(InvocationState::Cancelled),
        "timed_out" => Ok(InvocationState::TimedOut),
        "interrupted" => Ok(InvocationState::Interrupted),
        _ => Err(format!("unsupported invocation state: {value}")),
    }
}

fn inspect_visible_budget(requested: Option<u32>) -> usize {
    requested.unwrap_or(8 * 1024).clamp(512, 8 * 1024) as usize
}

#[cfg(test)]
mod tests {
    use bazel_mcp_runner::RunnerConfig;
    use bazel_mcp_store::Store;
    use bazel_mcp_types::{Diagnostic, Termination};
    use tempfile::tempdir;

    use super::*;

    #[tokio::test]
    async fn invocation_ledger_can_be_scoped_to_a_workspace() {
        let root = tempdir().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let workspace = PathBuf::from("/workspace/ledger-scope");
        let mut record = InvocationRecord::queued(InvocationRequest::new(
            workspace.clone(),
            BazelCommand::Info,
            vec!["release".to_owned()],
        ));
        record.state = InvocationState::Failed;
        let id = record.request.id;
        store.create_invocation(&record).await.unwrap();
        let mut succeeded = InvocationRecord::queued(InvocationRequest::new(
            workspace.clone(),
            BazelCommand::Build,
            vec!["//...".to_owned()],
        ));
        succeeded.state = InvocationState::Succeeded;
        store.create_invocation(&succeeded).await.unwrap();
        let runner = InvocationService::new(store, RunnerConfig::default()).unwrap();
        let server = BazelMcpServer::new(runner, ResultEncoding::Text);

        let result = server
            .bazel_inspect(Parameters(InspectParams {
                invocation_id: None,
                workspace: Some(workspace.to_string_lossy().into_owned()),
                state: Some("failed".to_owned()),
                command: None,
                view: "invocations".to_owned(),
                filter: None,
                limit: Some(20),
                max_bytes: Some(2_048),
                cursor: None,
            }))
            .await
            .unwrap();
        let Some(ContentBlock::Text(content)) = result.content.first() else {
            panic!("ledger result did not contain one text block");
        };
        assert!(content.text.len() <= 2_048);
        let value: serde_json::Value = serde_json::from_str(&content.text).unwrap();
        assert_eq!(value["view"], "invocations");
        assert_eq!(value["items"].as_array().unwrap().len(), 1);
        assert_eq!(value["items"][0]["invocation_id"], id.to_string());

        let result = server
            .bazel_inspect(Parameters(InspectParams {
                invocation_id: None,
                workspace: Some(workspace.to_string_lossy().into_owned()),
                state: None,
                command: Some("build".to_owned()),
                view: "invocations".to_owned(),
                filter: None,
                limit: Some(20),
                max_bytes: Some(2_048),
                cursor: None,
            }))
            .await
            .unwrap();
        let Some(ContentBlock::Text(content)) = result.content.first() else {
            panic!("command-filtered ledger result did not contain one text block");
        };
        let value: serde_json::Value = serde_json::from_str(&content.text).unwrap();
        assert_eq!(value["items"].as_array().unwrap().len(), 1);
        assert_eq!(value["items"][0]["command"], "build");

        let result = server
            .bazel_inspect(Parameters(InspectParams {
                invocation_id: Some(id.to_string()),
                workspace: None,
                state: None,
                command: None,
                view: "metrics".to_owned(),
                filter: None,
                limit: Some(20),
                max_bytes: Some(2_048),
                cursor: None,
            }))
            .await
            .unwrap();
        let Some(ContentBlock::Text(content)) = result.content.first() else {
            panic!("metrics result did not contain one text block");
        };
        let value: serde_json::Value = serde_json::from_str(&content.text).unwrap();
        assert_eq!(value["view"], "metrics");
        assert_eq!(value["items"][0]["state"], "failed");
    }

    #[test]
    fn inspection_byte_budget_is_bounded() {
        assert_eq!(inspect_visible_budget(None), 8_192);
        assert_eq!(inspect_visible_budget(Some(1)), 512);
        assert_eq!(inspect_visible_budget(Some(2_048)), 2_048);
        assert_eq!(inspect_visible_budget(Some(20_000)), 8_192);
    }

    #[tokio::test]
    async fn result_encodings_charge_their_model_visible_representations() {
        let root = tempdir().unwrap();
        let runner = InvocationService::new(
            Store::open(root.path()).await.unwrap(),
            RunnerConfig::default(),
        )
        .unwrap();
        let value = serde_json::json!({
            "view": "log",
            "items": ["ERROR: first", "ERROR: second"],
            "next_cursor": null,
            "truncated": false
        });
        let one = serde_json::to_vec(&value).unwrap().len();

        let text = BazelMcpServer::new(runner.clone(), ResultEncoding::Text);
        assert_eq!(text.model_visible_bytes(&value).unwrap(), one);
        let text_result = text.encode(value.clone()).unwrap();
        let Some(ContentBlock::Text(content)) = text_result.content.first() else {
            panic!("text result did not contain one text block");
        };
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&content.text).unwrap(),
            value
        );

        let toon = BazelMcpServer::new(runner.clone(), ResultEncoding::Toon);
        let toon_text = BazelMcpServer::encode_toon(&value).unwrap();
        assert_eq!(toon.model_visible_bytes(&value).unwrap(), toon_text.len());
        let decoded: serde_json::Value = toon_format::decode_default(&toon_text).unwrap();
        assert_eq!(decoded, value);
        let result = toon.encode(value.clone()).unwrap();
        assert_eq!(result.structured_content, None);
        assert_eq!(result.content.len(), 1);
        let Some(ContentBlock::Text(content)) = result.content.first() else {
            panic!("TOON result did not contain one text block");
        };
        assert_eq!(content.text, toon_text);

        let structured = BazelMcpServer::new(runner.clone(), ResultEncoding::Structured);
        assert_eq!(
            structured.encode(value.clone()).unwrap().structured_content,
            Some(value.clone())
        );

        let both = BazelMcpServer::new(runner, ResultEncoding::Both);
        assert_eq!(both.model_visible_bytes(&value).unwrap(), one * 2);
        assert_eq!(
            both.encode(value.clone()).unwrap().structured_content,
            Some(value)
        );
    }

    #[tokio::test]
    async fn failed_run_response_keeps_primary_rust_test_evidence_under_budget() {
        let root = tempdir().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let mut record = InvocationRecord::queued(InvocationRequest::new(
            PathBuf::from("/workspace/rust-failure"),
            BazelCommand::Test,
            vec!["//pkg:unit_test".to_owned()],
        ));
        record.state = InvocationState::Failed;
        record.termination = Some(Termination::Exit { code: 3 });
        let primary = "Rust test tests::parses_value failed at src/test.rs:17:9: assertion `left == right` failed; left: 1; right: 2";
        let mut diagnostics = vec![Diagnostic {
            severity: bazel_mcp_types::Severity::Error,
            category: bazel_mcp_types::DiagnosticCategory::Test,
            message: primary.to_owned(),
            location: Some(bazel_mcp_types::DiagnosticLocation {
                path: "src/test.rs".to_owned(),
                line: Some(17),
                column: Some(9),
            }),
            target: Some("//pkg:unit_test".to_owned()),
            action: None,
            repetition_count: 1,
        }];
        diagnostics.extend((0..30).map(|index| Diagnostic {
            severity: bazel_mcp_types::Severity::Error,
            category: bazel_mcp_types::DiagnosticCategory::Action,
            message: format!("noise-{index}-{}", "x".repeat(900)),
            location: None,
            target: None,
            action: None,
            repetition_count: 1,
        }));
        record.summary = Some(bazel_mcp_types::InvocationSummary {
            success: false,
            headline: format!("Bazel failed: {primary}"),
            diagnostics,
            test_counts: bazel_mcp_types::TestCounts {
                failed: 1,
                ..bazel_mcp_types::TestCounts::default()
            },
            ..bazel_mcp_types::InvocationSummary::default()
        });
        store.create_invocation(&record).await.unwrap();
        let runner = InvocationService::new(store, RunnerConfig::default()).unwrap();
        let server = BazelMcpServer::new(runner, ResultEncoding::Text);

        let result = server.build_run_result(&record, false).await.unwrap();
        let Some(ContentBlock::Text(content)) = result.content.first() else {
            panic!("run result did not contain one text block");
        };
        let value: serde_json::Value = serde_json::from_str(&content.text).unwrap();
        assert_eq!(value["headline"], format!("Bazel failed: {primary}"));
        assert_eq!(value["diagnostics"][0]["message"], primary);
        assert_eq!(value["diagnostics"][0]["category"], "test");
        assert_eq!(value["diagnostics"][0]["location"]["line"], 17);
        assert_eq!(value["tests"]["failed"], 1);
        assert_eq!(value["truncated"], true);
        assert!(content.text.len() <= 8 * 1024);
    }

    #[tokio::test]
    async fn negotiated_capabilities_and_tool_metadata_do_not_mix_dialects() {
        let root = tempdir().unwrap();
        let runner = InvocationService::new(
            Store::open(root.path()).await.unwrap(),
            RunnerConfig::default(),
        )
        .unwrap();
        let server = BazelMcpServer::new(runner, ResultEncoding::Text).with_task_execution(
            McpExecutionPolicy::Auto,
            Duration::from_secs(60),
            500,
        );

        let legacy = serde_json::to_value(
            server
                .initialize_result(ProtocolVersion::V_2025_11_25)
                .capabilities,
        )
        .unwrap();
        assert_eq!(
            legacy,
            serde_json::json!({
                "tools": {},
                "tasks": {
                    "list": {},
                    "cancel": {},
                    "requests": {"tools": {"call": {}}}
                }
            })
        );
        let legacy_tools = server.tools_for(ProtocolFamily::LegacyTasks);
        assert_eq!(legacy_tools.len(), 3);
        assert_eq!(
            legacy_tools
                .iter()
                .find(|tool| tool.name == "bazel.run")
                .unwrap()
                .task_support(),
            TaskSupport::Optional
        );
        assert!(
            legacy_tools
                .iter()
                .filter(|tool| tool.name != "bazel.run")
                .all(|tool| tool.execution.is_none())
        );

        let extension_version: ProtocolVersion = serde_json::from_str("\"2026-06-30\"").unwrap();
        let extension =
            serde_json::to_value(server.initialize_result(extension_version).capabilities).unwrap();
        assert_eq!(
            extension,
            serde_json::json!({
                "tools": {},
                "extensions": {(TASKS_EXTENSION): {}}
            })
        );
        assert!(
            server
                .tools_for(ProtocolFamily::TasksExtension)
                .iter()
                .all(|tool| tool.execution.is_none())
        );
        assert_eq!(
            serde_json::to_value(BazelMcpServer::missing_extension_capability()).unwrap(),
            serde_json::json!({
                "code": -32003,
                "message": "Missing Required Client Capability",
                "data": {
                    "requiredCapabilities": {
                        "extensions": {(TASKS_EXTENSION): {}}
                    }
                }
            })
        );
    }
}
