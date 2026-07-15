use std::{
    collections::HashMap,
    io,
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    process::ExitStatus,
    sync::{Arc, Weak},
    time::{Duration, Instant},
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use bazel_mcp_bep::PartialStream;
use bazel_mcp_policy::{
    PolicyConfig, PolicyError, Redactor, effective_output_base, filtered_environment,
    resolve_bazel_executable, validate_arguments, validate_command, validate_query_arguments,
    validate_workspace,
};
use bazel_mcp_reducer::{
    Budget, ReductionInput, extract_canonical_arguments, normalize_terminal_text,
    parse_lcov_reader, parse_test_xml, reduce_artifacts, reduce_invocation,
};
use bazel_mcp_store::{InvocationPaths, Store, StoreError};
use bazel_mcp_types::{
    ArtifactKind, BazelCommand, CommandClass, Diagnostic, DiagnosticCategory, InvocationId,
    InvocationMetrics, InvocationRecord, InvocationRequest, InvocationState, PageRequest, QueryRow,
    Severity, Termination, TestStatus,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, BufReader},
    process::Command,
    sync::{Mutex, Semaphore},
};
use tokio_util::sync::CancellationToken;

use crate::{cancel::terminate_child, capture};

const REDUCTION_LOG_LIMIT: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct RunnerConfig {
    pub policy: PolicyConfig,
    pub default_timeout: Duration,
    pub maximum_timeout: Duration,
    pub cancellation_interrupt_grace: Duration,
    pub cancellation_terminate_grace: Duration,
    pub global_concurrency: usize,
    pub output_user_root: Option<PathBuf>,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            policy: PolicyConfig::default(),
            default_timeout: Duration::from_secs(30 * 60),
            maximum_timeout: Duration::from_secs(2 * 60 * 60),
            cancellation_interrupt_grace: Duration::from_secs(10),
            cancellation_terminate_grace: Duration::from_secs(5),
            global_concurrency: 4,
            output_user_root: None,
        }
    }
}

#[derive(Debug, Error)]
pub enum RunnerError {
    #[error(transparent)]
    Policy(#[from] PolicyError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("runner task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
    #[error("global invocation semaphore was closed")]
    SchedulerClosed,
    #[error("invalid runner configuration: {0}")]
    InvalidConfiguration(&'static str),
    #[error("invocation is already terminal: {0}")]
    AlreadyTerminal(InvocationId),
    #[error("requested log cursor is invalid or outside the file")]
    InvalidOffset,
    #[error("inspect response cannot fit the requested {0}-byte limit")]
    ResponseTooLarge(usize),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[derive(Clone)]
pub struct InvocationService {
    store: Store,
    config: RunnerConfig,
    redactor: Redactor,
    global: Arc<Semaphore>,
    workspace_locks: Arc<Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>>,
    live: Arc<Mutex<HashMap<InvocationId, CancellationToken>>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InspectView {
    Summary,
    Diagnostics,
    Tests,
    Coverage,
    Artifacts,
    QueryResults,
    Log,
    Invocations,
}

#[derive(Clone, Debug)]
pub struct InspectRequest {
    pub invocation_id: Option<InvocationId>,
    pub workspace: Option<PathBuf>,
    pub view: InspectView,
    pub cursor: Option<String>,
    pub filter: Option<String>,
    pub limit: u32,
    pub max_bytes: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct LogCursor {
    invocation_id: InvocationId,
    end: u64,
}

impl LogCursor {
    fn encode(&self) -> Result<String, RunnerError> {
        Ok(URL_SAFE_NO_PAD.encode(serde_json::to_vec(self)?))
    }

    fn decode(value: &str) -> Result<Self, RunnerError> {
        let raw = URL_SAFE_NO_PAD
            .decode(value)
            .map_err(|_| RunnerError::InvalidOffset)?;
        serde_json::from_slice(&raw).map_err(|_| RunnerError::InvalidOffset)
    }

    fn decode_for(value: &str, invocation_id: InvocationId) -> Result<Self, RunnerError> {
        let cursor = Self::decode(value)?;
        if cursor.invocation_id != invocation_id {
            return Err(RunnerError::InvalidOffset);
        }
        Ok(cursor)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct InspectResult {
    pub invocation_id: Option<InvocationId>,
    pub view: InspectView,
    pub items: serde_json::Value,
    pub total_count: Option<u64>,
    pub filtered_count: Option<u64>,
    pub next_cursor: Option<String>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct CancelResult {
    pub invocation_id: InvocationId,
    pub prior_state: InvocationState,
    pub current_state: InvocationState,
    pub cancellation_requested: bool,
}

impl InvocationService {
    pub fn new(store: Store, config: RunnerConfig) -> Result<Self, RunnerError> {
        if config.global_concurrency == 0 {
            return Err(RunnerError::InvalidConfiguration(
                "global concurrency must be greater than zero",
            ));
        }
        if config.maximum_timeout.is_zero() {
            return Err(RunnerError::InvalidConfiguration(
                "maximum timeout must be greater than zero",
            ));
        }
        if config.default_timeout > config.maximum_timeout {
            return Err(RunnerError::InvalidConfiguration(
                "default timeout exceeds maximum timeout",
            ));
        }
        let redactor = Redactor::new(&config.policy.redaction_patterns)?;
        Ok(Self {
            store,
            global: Arc::new(Semaphore::new(config.global_concurrency)),
            config,
            redactor,
            workspace_locks: Arc::new(Mutex::new(HashMap::new())),
            live: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    #[must_use]
    pub fn store(&self) -> &Store {
        &self.store
    }

    pub async fn run(&self, request: InvocationRequest) -> Result<InvocationRecord, RunnerError> {
        self.run_with_cancellation(request, CancellationToken::new())
            .await
    }

    pub async fn run_with_cancellation(
        &self,
        mut request: InvocationRequest,
        cancellation: CancellationToken,
    ) -> Result<InvocationRecord, RunnerError> {
        validate_command(&self.config.policy, &request.command)?;
        validate_arguments(&request.startup_arguments)?;
        validate_arguments(&request.arguments)?;
        validate_query_arguments(&request.command, &request.arguments)?;
        let workspace = validate_workspace(&request.workspace, &self.config.policy.allowed_roots)?;
        let lock_key = effective_output_base(&workspace, &request.startup_arguments)?
            .unwrap_or_else(|| workspace.clone());
        let executable = resolve_bazel_executable(&workspace, &self.config.policy)?;
        request.workspace = workspace.clone();
        let id = request.id;
        let queued = InvocationRecord::queued(request);
        let stored = InvocationRecord::queued(self.redacted_request(&queued.request));
        let paths = self.store.create_invocation(&stored).await?;
        self.live.lock().await.insert(id, cancellation.clone());

        let result = async {
            let workspace_lock = self.workspace_lock(&lock_key).await;
            let workspace_guard = tokio::select! {
                guard = workspace_lock.lock() => guard,
                () = cancellation.cancelled() => {
                    return self.finish_cancelled(id).await;
                }
            };
            let permit = tokio::select! {
                permit = self.global.clone().acquire_owned() => {
                    permit.map_err(|_| RunnerError::SchedulerClosed)?
                }
                () = cancellation.cancelled() => {
                    return self.finish_cancelled(id).await;
                }
            };

            if cancellation.is_cancelled() {
                return self.finish_cancelled(id).await;
            }
            self.store
                .transition(id, InvocationState::Starting, None, None)
                .await?;
            let result = self
                .execute(&queued, &paths, &executable, cancellation.clone())
                .await;
            drop(workspace_guard);
            drop(permit);
            result
        }
        .await;
        self.live.lock().await.remove(&id);
        result
    }

    pub async fn cancel(&self, id: InvocationId) -> Result<CancelResult, RunnerError> {
        self.cancel_with_reason(id, None).await
    }

    pub async fn cancel_with_reason(
        &self,
        id: InvocationId,
        reason: Option<&str>,
    ) -> Result<CancelResult, RunnerError> {
        let record = self.store.get_invocation(id).await?;
        if record.state.is_terminal() {
            return Ok(CancelResult {
                invocation_id: id,
                prior_state: record.state,
                current_state: record.state,
                cancellation_requested: false,
            });
        }
        if let Some(reason) = reason {
            let reason = self.redactor.redact_bounded(reason, 1_000);
            self.store.update_cancellation_reason(id, &reason).await?;
        }
        let cancellation = self.live.lock().await.get(&id).cloned();
        if let Some(cancellation) = &cancellation {
            cancellation.cancel();
        }
        if record.state == InvocationState::Queued {
            match self
                .store
                .transition(
                    id,
                    InvocationState::Cancelled,
                    Some(Termination::Cancelled),
                    Some(cancelled_summary()),
                )
                .await
            {
                Ok(cancelled) => {
                    return Ok(CancelResult {
                        invocation_id: id,
                        prior_state: record.state,
                        current_state: cancelled.state,
                        cancellation_requested: true,
                    });
                }
                Err(StoreError::State(_)) => {
                    // The scheduler won the race and began starting the
                    // process. Its cancellation token takes the graceful
                    // process-group path below.
                }
                Err(error) => return Err(error.into()),
            }
        }
        let current_state = self.store.get_invocation(id).await?.state;
        Ok(CancelResult {
            invocation_id: id,
            prior_state: record.state,
            current_state,
            cancellation_requested: cancellation.is_some(),
        })
    }

    pub async fn record_model_visible_result(
        &self,
        id: InvocationId,
        bytes: usize,
        inspection: bool,
    ) -> Result<(), RunnerError> {
        self.store
            .record_model_visible_result(id, bytes as u64, inspection)
            .await?;
        Ok(())
    }

    pub async fn record_progress_notifications(
        &self,
        id: InvocationId,
        count: u64,
    ) -> Result<(), RunnerError> {
        self.store.record_progress_notifications(id, count).await?;
        Ok(())
    }

    pub async fn invocation_state(&self, id: InvocationId) -> Result<InvocationState, RunnerError> {
        Ok(self.store.get_invocation(id).await?.state)
    }

    pub async fn inspect(&self, request: InspectRequest) -> Result<InspectResult, RunnerError> {
        if request.view == InspectView::Invocations {
            let page = self
                .store
                .list_invocations(
                    request.workspace.as_deref(),
                    PageRequest {
                        cursor: request.cursor,
                        limit: request.limit.min(1),
                    },
                )
                .await?;
            let next_cursor = page.next_cursor.clone();
            let truncated = page.truncated;
            return enforce_inspect_budget(
                InspectResult {
                    invocation_id: None,
                    view: request.view,
                    items: serde_json::to_value(page.items)?,
                    total_count: None,
                    filtered_count: None,
                    next_cursor,
                    truncated,
                },
                request.max_bytes,
            );
        }

        let id = request.invocation_id.ok_or(StoreError::InvalidCursor)?;
        let record = self.store.get_invocation(id).await?;
        let page_request = PageRequest {
            cursor: request.cursor.clone(),
            limit: bounded_page_limit(request.view, request.limit, request.max_bytes),
        };
        let paths = self.store.paths_for(&record);
        let (items, total_count, filtered_count, next_cursor, truncated) = match request.view {
            InspectView::Summary => {
                let items = record.summary.as_ref().map_or_else(Vec::new, |summary| {
                    vec![serde_json::json!({
                        "success": summary.success,
                        "headline": summary.headline,
                        "targets": summary.target_counts,
                        "tests": summary.test_counts,
                        "diagnostics": summary.diagnostics,
                        "coverage": summary.coverage.as_ref().map(|coverage| serde_json::json!({
                            "lines_found": coverage.lines_found,
                            "lines_hit": coverage.lines_hit,
                            "coverage_percent": coverage.coverage_percent,
                        })),
                        "query_result_count": summary.query_result_count,
                        "query_sample": summary.query_sample,
                        "elapsed_ms": summary.elapsed_ms,
                        "truncated": summary.truncated,
                        "inspect_hint": summary.inspect_hint,
                    })]
                });
                (
                    serde_json::to_value(items)?,
                    Some(u64::from(record.summary.is_some())),
                    Some(u64::from(record.summary.is_some())),
                    None,
                    record
                        .summary
                        .as_ref()
                        .is_some_and(|summary| summary.truncated),
                )
            }
            InspectView::Diagnostics => {
                let (page, total, filtered) = self
                    .store
                    .page_diagnostics(id, request.filter.as_deref(), page_request)
                    .await?;
                (
                    serde_json::to_value(page.items)?,
                    Some(total),
                    Some(filtered),
                    page.next_cursor,
                    page.truncated,
                )
            }
            InspectView::Tests => {
                let (page, total, filtered) = self
                    .store
                    .page_tests(id, request.filter.as_deref(), page_request)
                    .await?;
                (
                    serde_json::to_value(page.items)?,
                    Some(total),
                    Some(filtered),
                    page.next_cursor,
                    page.truncated,
                )
            }
            InspectView::Artifacts => {
                let (page, total, filtered) = self
                    .store
                    .page_artifacts(id, request.filter.as_deref(), page_request)
                    .await?;
                (
                    serde_json::to_value(page.items)?,
                    Some(total),
                    Some(filtered),
                    page.next_cursor,
                    page.truncated,
                )
            }
            InspectView::QueryResults => {
                let (page, total, filtered) = self
                    .store
                    .page_query_rows(id, request.filter.as_deref(), page_request)
                    .await?;
                (
                    serde_json::to_value(page.items)?,
                    Some(total),
                    Some(filtered),
                    page.next_cursor,
                    page.truncated,
                )
            }
            InspectView::Coverage => {
                let (page, total, filtered) = self
                    .store
                    .page_coverage(id, request.filter.as_deref(), page_request)
                    .await?;
                let mut items = serde_json::to_value(page.items)?;
                let mut total = total;
                let mut filtered = filtered;
                if items.as_array().is_some_and(Vec::is_empty) {
                    let (artifacts, _, _) = self
                        .store
                        .page_artifacts(
                            id,
                            request.filter.as_deref(),
                            PageRequest {
                                cursor: None,
                                limit: request.limit,
                            },
                        )
                        .await?;
                    let unavailable = artifacts
                        .items
                        .into_iter()
                        .filter(|artifact| {
                            artifact.kind == ArtifactKind::Coverage && !artifact.locally_available
                        })
                        .map(|artifact| {
                            serde_json::json!({
                                "availability_reason": "remote_artifact_unavailable",
                                "artifact": artifact,
                            })
                        })
                        .collect::<Vec<_>>();
                    let unavailable = if unavailable.is_empty() {
                        vec![serde_json::json!({
                            "availability_reason": "coverage_artifact_not_found",
                        })]
                    } else {
                        unavailable
                    };
                    total = unavailable.len() as u64;
                    filtered = total;
                    items = serde_json::Value::Array(unavailable);
                }
                (
                    items,
                    Some(total),
                    Some(filtered),
                    page.next_cursor,
                    page.truncated,
                )
            }
            InspectView::Log => {
                let (content, truncated, next_cursor) =
                    self.read_combined_log_page(&paths, &request).await?;
                (
                    serde_json::json!([content]),
                    None,
                    None,
                    next_cursor,
                    truncated,
                )
            }
            InspectView::Invocations => unreachable!("handled above"),
        };
        enforce_inspect_budget(
            InspectResult {
                invocation_id: Some(id),
                view: request.view,
                items,
                total_count,
                filtered_count,
                next_cursor,
                truncated,
            },
            request.max_bytes,
        )
    }

    async fn execute(
        &self,
        queued: &InvocationRecord,
        paths: &InvocationPaths,
        executable: &Path,
        cancellation: CancellationToken,
    ) -> Result<InvocationRecord, RunnerError> {
        if cancellation.is_cancelled() {
            return self.finish_cancelled(queued.request.id).await;
        }
        let queue_ms = u64::try_from(
            bazel_mcp_types::unix_timestamp_ms().saturating_sub(queued.request.requested_at_ms),
        )
        .unwrap_or_default();
        let (stdout, stderr) = capture::open_stdio(paths).await?;
        let mut command = Command::new(executable);
        command
            .current_dir(&queued.request.workspace)
            .env_clear()
            .envs(filtered_environment(&self.config.policy));
        if let Some(output_user_root) = &self.config.output_user_root {
            command.arg(format!("--output_user_root={}", output_user_root.display()));
        }
        command
            .args(&queued.request.startup_arguments)
            .arg(queued.request.command.as_str());
        if queued.request.command.class() == CommandClass::BuildLike {
            command
                .arg(format!("--invocation_id={}", queued.request.id))
                .arg(format!("--build_event_binary_file={}", paths.bep.display()))
                .args([
                    "--build_event_binary_file_path_conversion=false",
                    "--tool_tag=bazel-mcp",
                    "--color=no",
                    "--curses=no",
                    "--show_progress=false",
                    "--show_result=0",
                ]);
            if queued.request.command == BazelCommand::Test {
                command.args(["--test_output=summary", "--test_summary=none"]);
            }
        }
        command.args(&queued.request.arguments);
        command.stdout(stdout).stderr(stderr).kill_on_drop(true);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.as_std_mut().process_group(0);
        }

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let message = self.redactor.redact_bounded(&error.to_string(), 1_000);
                let summary = bazel_mcp_types::InvocationSummary {
                    success: false,
                    headline: format!("Could not start Bazel: {message}"),
                    ..Default::default()
                };
                return self
                    .store
                    .transition(
                        queued.request.id,
                        InvocationState::Failed,
                        Some(Termination::SpawnFailure { message }),
                        Some(summary),
                    )
                    .await
                    .map_err(Into::into);
            }
        };
        if let Err(error) = self
            .store
            .transition(queued.request.id, InvocationState::Running, None, None)
            .await
        {
            let _ = terminate_child(
                &mut child,
                self.config.cancellation_interrupt_grace,
                self.config.cancellation_terminate_grace,
            )
            .await;
            return Err(error.into());
        }
        let started = Instant::now();
        let timeout = queued
            .request
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(self.config.default_timeout)
            .max(Duration::from_secs(1))
            .min(self.config.maximum_timeout);
        let timeout_sleep = tokio::time::sleep(timeout);
        tokio::pin!(timeout_sleep);
        let (status, termination, state) = tokio::select! {
            result = child.wait() => finish_from_status(result?),
            () = cancellation.cancelled() => {
                let status = terminate_child(
                    &mut child,
                    self.config.cancellation_interrupt_grace,
                    self.config.cancellation_terminate_grace,
                ).await?;
                (Some(status), Termination::Cancelled, InvocationState::Cancelled)
            }
            () = &mut timeout_sleep => {
                let status = terminate_child(
                    &mut child,
                    self.config.cancellation_interrupt_grace,
                    self.config.cancellation_terminate_grace,
                ).await?;
                (Some(status), Termination::Timeout, InvocationState::TimedOut)
            }
        };
        let bazel_wall_ms = duration_millis(started.elapsed());
        let postprocess: Result<InvocationRecord, RunnerError> = async {
            let reduction_started = Instant::now();
            let (query_row_count, query_sample) =
                if queued.request.command.class() == CommandClass::Query {
                    self.persist_query_rows(queued.request.id, &paths.stdout)
                        .await?
                } else {
                    (0, Vec::new())
                };
            let stdout = capture::read_bounded_tail(&paths.stdout, REDUCTION_LOG_LIMIT).await?;
            let stderr = capture::read_bounded_tail(&paths.stderr, REDUCTION_LOG_LIMIT).await?;
            let PartialStream {
                events,
                terminal_error,
            } = capture::read_bep(paths.bep.clone()).await?;
            if let Some(error) = terminal_error {
                tracing::warn!(invocation_id = %queued.request.id, %error, "partially decoded BEP");
            }
            if let Some(mut arguments) =
                catch_unwind(AssertUnwindSafe(|| extract_canonical_arguments(&events)))
                    .ok()
                    .flatten()
            {
                let workspace = queued.request.workspace.to_string_lossy();
                for argument in &mut arguments {
                    *argument = self.redactor.redact_bounded(
                        &argument.replace(workspace.as_ref(), "<workspace>"),
                        64 * 1024,
                    );
                }
                self.store
                    .update_canonical_arguments(queued.request.id, &arguments)
                    .await?;
            }
            let mut artifacts = catch_unwind(AssertUnwindSafe(|| reduce_artifacts(&events)))
                .unwrap_or_else(|_| {
                    tracing::warn!(
                        invocation_id = %queued.request.id,
                        "artifact reducer panicked; continuing without structured artifacts"
                    );
                    Vec::new()
                });
            for artifact in &mut artifacts {
                artifact.name = self.redactor.redact_bounded(&artifact.name, 1_000);
                artifact.uri = self.redactor.redact_bounded(&artifact.uri, 1_000);
            }
            self.store
                .replace_artifacts(queued.request.id, &artifacts)
                .await?;
            let exit_code = status.as_ref().and_then(ExitStatus::code);
            let mut summary = catch_unwind(AssertUnwindSafe(|| {
                reduce_invocation(ReductionInput {
                    events: &events,
                    stdout: &stdout,
                    stderr: &stderr,
                    exit_code,
                    elapsed_ms: bazel_wall_ms,
                    budget: Budget::result_default(),
                })
            }))
            .unwrap_or_else(|_| {
                tracing::warn!(
                    invocation_id = %queued.request.id,
                    "invocation reducer panicked; using bounded fallback summary"
                );
                fallback_summary(exit_code, bazel_wall_ms, &stderr, &stdout)
            });
            if queued.request.command.class() == CommandClass::Query && exit_code == Some(0) {
                summary.headline = format!("Bazel query returned {query_row_count} rows");
                summary.inspect_hint = (query_row_count > 0).then(|| "query_results".to_owned());
                summary.query_result_count = Some(query_row_count);
                summary.query_sample = query_sample;
            } else if matches!(
                queued.request.command.class(),
                CommandClass::Informational | CommandClass::Unknown
            ) && exit_code == Some(0)
            {
                let text = if stdout.is_empty() { &stderr } else { &stdout };
                let excerpt = normalize_terminal_text(text);
                if !excerpt.is_empty() {
                    summary.headline = bounded_text(&excerpt, 1_000);
                    if excerpt.len() > 1_000 {
                        summary.truncated = true;
                        summary.inspect_hint = Some("log".to_owned());
                    }
                }
            }
            self.enrich_tests(&queued.request.workspace, &mut summary, &artifacts)
                .await;
            if queued.request.command == BazelCommand::Coverage {
                summary.coverage = self
                    .load_coverage(&queued.request.workspace, &artifacts)
                    .await;
            }
            self.sanitize_summary(queued.request.id, &queued.request.workspace, &mut summary);
            let metrics = InvocationMetrics {
                raw_stdout_bytes: capture::file_size(&paths.stdout).await,
                raw_stderr_bytes: capture::file_size(&paths.stderr).await,
                bep_bytes: capture::file_size(&paths.bep).await,
                bep_events: u64::try_from(events.len()).unwrap_or(u64::MAX),
                queue_ms,
                bazel_wall_ms,
                reduction_ms: duration_millis(reduction_started.elapsed()),
                ..Default::default()
            };
            self.store
                .update_metrics(queued.request.id, metrics)
                .await?;
            self.store
                .transition(
                    queued.request.id,
                    state,
                    Some(termination.clone()),
                    Some(summary),
                )
                .await
                .map_err(Into::into)
        }
        .await;

        if let Err(error) = &postprocess {
            let workspace = queued.request.workspace.to_string_lossy();
            let message = self.redactor.redact_bounded(
                &error.to_string().replace(workspace.as_ref(), "<workspace>"),
                1_000,
            );
            tracing::warn!(
                invocation_id = %queued.request.id,
                error = %message,
                "Bazel result post-processing failed"
            );
            if self
                .store
                .get_invocation(queued.request.id)
                .await
                .is_ok_and(|record| !record.state.is_terminal())
            {
                let terminal_state = if state == InvocationState::Succeeded {
                    InvocationState::Failed
                } else {
                    state
                };
                let summary = bazel_mcp_types::InvocationSummary {
                    success: false,
                    headline: format!("Could not finish processing Bazel results: {message}"),
                    elapsed_ms: bazel_wall_ms,
                    truncated: true,
                    inspect_hint: Some("log".to_owned()),
                    ..Default::default()
                };
                let _ = self
                    .store
                    .transition(
                        queued.request.id,
                        terminal_state,
                        Some(termination),
                        Some(summary),
                    )
                    .await;
            }
            if let Ok(record) = self.store.get_invocation(queued.request.id).await
                && record.state.is_terminal()
                && record.summary.is_some()
            {
                return Ok(record);
            }
        }
        postprocess
    }

    async fn workspace_lock(&self, workspace: &Path) -> Arc<Mutex<()>> {
        let mut locks = self.workspace_locks.lock().await;
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(workspace).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(Mutex::new(()));
        locks.insert(workspace.to_owned(), Arc::downgrade(&lock));
        lock
    }

    fn sanitize_summary(
        &self,
        id: InvocationId,
        workspace: &Path,
        summary: &mut bazel_mcp_types::InvocationSummary,
    ) {
        let workspace = workspace.to_string_lossy();
        let sanitize = |value: &str, maximum_bytes| {
            self.redactor.redact_bounded(
                &value.replace(workspace.as_ref(), "<workspace>"),
                maximum_bytes,
            )
        };
        summary.headline = sanitize(&summary.headline, 1_000);
        for target in &mut summary.targets {
            target.label = sanitize(&target.label, 1_000);
        }
        for diagnostic in &mut summary.diagnostics {
            diagnostic.message = sanitize(&diagnostic.message, 1_000);
            if let Some(location) = &mut diagnostic.location {
                location.path = sanitize(&location.path, 1_000);
            }
            diagnostic.target = diagnostic
                .target
                .as_deref()
                .map(|target| sanitize(target, 1_000));
            diagnostic.action = diagnostic
                .action
                .as_deref()
                .map(|action| sanitize(action, 1_000));
        }
        if summary.diagnostics.len() > 20 {
            summary.diagnostics.truncate(20);
            summary.truncated = true;
            summary.inspect_hint = Some("diagnostics".to_owned());
        }
        for (index, test) in summary.tests.iter_mut().enumerate() {
            test.label = sanitize(&test.label, 1_000);
            for case in &mut test.cases {
                case.name = sanitize(&case.name, 512);
                case.message = case
                    .message
                    .as_deref()
                    .map(|message| sanitize(message, 1_000));
            }
            test.log_uri = (test.status != TestStatus::Passed)
                .then(|| format!("bazel://invocations/{id}/tests/{index}/failure-log"));
        }
        if let Some(coverage) = &mut summary.coverage {
            for file in &mut coverage.files {
                file.path = sanitize(&file.path, 1_000);
            }
        }
    }

    async fn finish_cancelled(&self, id: InvocationId) -> Result<InvocationRecord, RunnerError> {
        let current = self.store.get_invocation(id).await?;
        if current.state.is_terminal() {
            return Ok(current);
        }
        self.store
            .transition(
                id,
                InvocationState::Cancelled,
                Some(Termination::Cancelled),
                Some(cancelled_summary()),
            )
            .await
            .map_err(Into::into)
    }

    fn redacted_request(&self, request: &InvocationRequest) -> InvocationRequest {
        let mut request = request.clone();
        for argument in request
            .startup_arguments
            .iter_mut()
            .chain(request.arguments.iter_mut())
        {
            *argument = self.redactor.redact_bounded(argument, 64 * 1024);
        }
        for value in request.environment.values_mut() {
            *value = self.redactor.redact_bounded(value, 64 * 1024);
        }
        request
    }

    async fn enrich_tests(
        &self,
        workspace: &Path,
        summary: &mut bazel_mcp_types::InvocationSummary,
        artifacts: &[bazel_mcp_types::Artifact],
    ) {
        let failed_test = summary
            .tests
            .iter_mut()
            .find(|test| test.status != TestStatus::Passed);
        let Some(test) = failed_test else {
            return;
        };
        if let Some(xml) = artifacts.iter().find(|artifact| {
            artifact.kind == ArtifactKind::TestLog && artifact.name.ends_with("test.xml")
        }) && let Some(path) = self.validated_artifact_path(workspace, xml).await
        {
            let small_enough = tokio::fs::metadata(&path)
                .await
                .is_ok_and(|metadata| metadata.len() <= 16 * 1024 * 1024);
            if small_enough
                && let Ok(contents) = tokio::fs::read_to_string(path).await
                && let Ok(cases) = parse_test_xml(&contents)
            {
                test.cases = cases
                    .into_iter()
                    .filter(|case| case.status != TestStatus::Passed)
                    .take(20)
                    .map(|mut case| {
                        case.name = bounded_text(&case.name, 512);
                        case.message = case.message.map(|message| bounded_text(&message, 1_000));
                        case
                    })
                    .collect();
            }
        }
        if test.cases.is_empty()
            && let Some(log) = artifacts.iter().find(|artifact| {
                artifact.kind == ArtifactKind::TestLog && artifact.name.ends_with("test.log")
            })
            && let Some(path) = self.validated_artifact_path(workspace, log).await
            && let Ok(bytes) = capture::read_bounded_tail(&path, 64 * 1024).await
        {
            let text = normalize_terminal_text(&bytes);
            if let Some(line) = text.lines().rev().find(|line| {
                let lower = line.to_ascii_lowercase();
                lower.contains("root_cause") || lower.contains("error:") || lower.contains("failed")
            }) {
                summary.diagnostics.push(Diagnostic {
                    severity: Severity::Error,
                    category: DiagnosticCategory::Test,
                    message: bounded_text(line, 1_000),
                    location: None,
                    target: Some(test.label.clone()),
                    action: None,
                    repetition_count: 1,
                });
            }
        }
        test.log_uri = Some("bazel://invocation/tests/failure-log".to_owned());
    }

    async fn load_coverage(
        &self,
        workspace: &Path,
        artifacts: &[bazel_mcp_types::Artifact],
    ) -> Option<bazel_mcp_types::CoverageSummary> {
        for artifact in artifacts
            .iter()
            .filter(|artifact| artifact.kind == ArtifactKind::Coverage)
        {
            let Some(canonical) = self.validated_artifact_path(workspace, artifact).await else {
                continue;
            };
            let parsed = tokio::task::spawn_blocking(move || {
                let file = std::fs::File::open(canonical)?;
                parse_lcov_reader(std::io::BufReader::new(file))
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
            })
            .await;
            if let Ok(Ok(coverage)) = parsed {
                return Some(coverage);
            }
        }
        None
    }

    async fn validated_artifact_path(
        &self,
        workspace: &Path,
        artifact: &bazel_mcp_types::Artifact,
    ) -> Option<PathBuf> {
        let path = local_artifact_path(artifact)?;
        let canonical = tokio::fs::canonicalize(path).await.ok()?;
        let in_workspace = canonical.starts_with(workspace);
        let in_output_root = if let Some(root) = &self.config.output_user_root {
            tokio::fs::canonicalize(root)
                .await
                .is_ok_and(|root| canonical.starts_with(root))
        } else {
            false
        };
        (in_workspace || in_output_root).then_some(canonical)
    }

    async fn persist_query_rows(
        &self,
        id: InvocationId,
        path: &Path,
    ) -> Result<(u64, Vec<QueryRow>), RunnerError> {
        self.store.replace_query_rows(id, &[]).await?;
        let file = tokio::fs::File::open(path).await?;
        let mut lines = BufReader::new(file).lines();
        let mut batch = Vec::with_capacity(512);
        let mut sample = Vec::with_capacity(3);
        let mut ordinal = 0_u64;
        while let Some(line) = lines.next_line().await? {
            let row = QueryRow {
                ordinal,
                value: self.redactor.redact_bounded(&line, 4 * 1024),
            };
            if sample.len() < 3 {
                sample.push(QueryRow {
                    ordinal,
                    value: bounded_text(&row.value, 256),
                });
            }
            batch.push(row);
            ordinal = ordinal.saturating_add(1);
            if batch.len() == 512 {
                self.store.append_query_rows(id, &batch).await?;
                batch.clear();
            }
        }
        if !batch.is_empty() {
            self.store.append_query_rows(id, &batch).await?;
        }
        Ok((ordinal, sample))
    }

    async fn read_combined_log_page(
        &self,
        paths: &InvocationPaths,
        request: &InspectRequest,
    ) -> Result<(Option<String>, bool, Option<String>), RunnerError> {
        let invocation_id = request.invocation_id.ok_or(StoreError::InvalidCursor)?;
        let stderr_length = tokio::fs::metadata(&paths.stderr)
            .await
            .map_or(0, |metadata| metadata.len());
        let path = if stderr_length > 0 {
            &paths.stderr
        } else {
            &paths.stdout
        };
        let length = tokio::fs::metadata(path)
            .await
            .map_or(0, |metadata| metadata.len());
        let end = request
            .cursor
            .as_deref()
            .map(|value| LogCursor::decode_for(value, invocation_id))
            .transpose()?
            .map_or(length, |cursor| cursor.end);
        if end > length {
            return Err(RunnerError::InvalidOffset);
        }
        let max_bytes = request.max_bytes.saturating_sub(512).clamp(1, 32 * 1024) as u64;
        let start = end.saturating_sub(max_bytes);
        let mut file = tokio::fs::File::open(path).await?;
        file.seek(std::io::SeekFrom::Start(start)).await?;
        let mut bytes = vec![0_u8; usize::try_from(end - start).unwrap_or(32 * 1024)];
        file.read_exact(&mut bytes).await?;
        let content = normalize_terminal_text(&bytes);
        let content = self
            .redactor
            .redact_bounded(&content, usize::try_from(max_bytes).unwrap_or(32 * 1024));
        let truncated = start > 0;
        let next_cursor = truncated
            .then_some(LogCursor {
                invocation_id,
                end: start,
            })
            .map(|cursor| cursor.encode())
            .transpose()?;
        Ok((Some(content), truncated, next_cursor))
    }
}

fn local_artifact_path(artifact: &bazel_mcp_types::Artifact) -> Option<PathBuf> {
    if !artifact.locally_available {
        return None;
    }
    if let Some(path) = artifact.uri.strip_prefix("file://") {
        return Some(PathBuf::from(path));
    }
    let path = PathBuf::from(&artifact.uri);
    path.is_absolute().then_some(path)
}

fn bounded_text(value: &str, maximum_bytes: usize) -> String {
    if value.len() <= maximum_bytes {
        return value.to_owned();
    }
    let mut boundary = maximum_bytes;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    format!("{}…", &value[..boundary])
}

fn bounded_page_limit(view: InspectView, requested: u32, max_bytes: usize) -> u32 {
    let estimated_item_bytes = match view {
        InspectView::Summary | InspectView::Tests | InspectView::Invocations => max_bytes.max(1),
        InspectView::Diagnostics => 5_000,
        InspectView::Coverage => 1_500,
        InspectView::Artifacts => 2_500,
        InspectView::QueryResults => 4_500,
        InspectView::Log => max_bytes.max(1),
    };
    let budgeted = u32::try_from(max_bytes / estimated_item_bytes)
        .unwrap_or(u32::MAX)
        .max(1);
    requested.clamp(1, 100).min(budgeted)
}

fn enforce_inspect_budget(
    mut result: InspectResult,
    requested_bytes: usize,
) -> Result<InspectResult, RunnerError> {
    let hard_limit = requested_bytes.min(32 * 1024);
    if serialized_len(&result)? <= hard_limit {
        return Ok(result);
    }
    result.truncated = true;

    match result.view {
        InspectView::Summary => {
            while serialized_len(&result)? > hard_limit {
                let Some(summary) = result
                    .items
                    .as_array_mut()
                    .and_then(|items| items.first_mut())
                else {
                    break;
                };
                let Some(diagnostics) = summary
                    .get_mut("diagnostics")
                    .and_then(serde_json::Value::as_array_mut)
                else {
                    break;
                };
                if diagnostics.pop().is_none() {
                    break;
                }
                summary["truncated"] = serde_json::Value::Bool(true);
            }
        }
        InspectView::Tests => {
            while serialized_len(&result)? > hard_limit {
                let Some(tests) = result.items.as_array_mut() else {
                    break;
                };
                let Some(cases) = tests.iter_mut().rev().find_map(|test| {
                    test.get_mut("cases")
                        .and_then(serde_json::Value::as_array_mut)
                        .filter(|cases| !cases.is_empty())
                }) else {
                    break;
                };
                cases.pop();
            }
        }
        _ => {}
    }

    for string_limit in [1_000, 512, 256, 64, 0] {
        if serialized_len(&result)? <= hard_limit {
            return Ok(result);
        }
        bound_json_strings(&mut result.items, string_limit);
    }
    if serialized_len(&result)? > hard_limit {
        return Err(RunnerError::ResponseTooLarge(hard_limit));
    }
    Ok(result)
}

fn serialized_len(result: &InspectResult) -> Result<usize, RunnerError> {
    Ok(serde_json::to_vec(result)?.len())
}

fn bound_json_strings(value: &mut serde_json::Value, maximum_bytes: usize) {
    match value {
        serde_json::Value::String(text) => {
            if maximum_bytes == 0 {
                text.clear();
            } else if text.len() > maximum_bytes {
                *text = bounded_text(text, maximum_bytes);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                bound_json_strings(item, maximum_bytes);
            }
        }
        serde_json::Value::Object(fields) => {
            for value in fields.values_mut() {
                bound_json_strings(value, maximum_bytes);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

fn finish_from_status(status: ExitStatus) -> (Option<ExitStatus>, Termination, InvocationState) {
    let state = if status.success() {
        InvocationState::Succeeded
    } else {
        InvocationState::Failed
    };
    if let Some(code) = status.code() {
        (Some(status), Termination::Exit { code }, state)
    } else {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            let signal = status.signal().unwrap_or_default();
            (Some(status), Termination::Signal { signal }, state)
        }
        #[cfg(not(unix))]
        {
            (Some(status), Termination::Exit { code: -1 }, state)
        }
    }
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn cancelled_summary() -> bazel_mcp_types::InvocationSummary {
    bazel_mcp_types::InvocationSummary {
        success: false,
        headline: "Bazel invocation was cancelled before starting".to_owned(),
        ..Default::default()
    }
}

fn fallback_summary(
    exit_code: Option<i32>,
    elapsed_ms: u64,
    stderr: &[u8],
    stdout: &[u8],
) -> bazel_mcp_types::InvocationSummary {
    let success = exit_code == Some(0);
    let text = if stderr.is_empty() { stdout } else { stderr };
    let message = normalize_terminal_text(text)
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(|line| bounded_text(line, 1_000));
    let diagnostics = if success {
        Vec::new()
    } else {
        message
            .as_ref()
            .map(|message| Diagnostic {
                severity: Severity::Error,
                category: DiagnosticCategory::Unknown,
                message: message.clone(),
                location: None,
                target: None,
                action: None,
                repetition_count: 1,
            })
            .into_iter()
            .collect()
    };
    bazel_mcp_types::InvocationSummary {
        success,
        headline: if success {
            format!("Bazel completed successfully in {elapsed_ms} ms")
        } else if let Some(message) = message {
            format!("Bazel failed: {message}")
        } else {
            format!("Bazel failed with exit code {exit_code:?}")
        },
        diagnostics,
        elapsed_ms,
        truncated: true,
        inspect_hint: Some("log".to_owned()),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[tokio::test]
    async fn workspace_lock_registry_discards_inactive_output_bases() {
        let root = tempdir().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let service = InvocationService::new(store, RunnerConfig::default()).unwrap();

        for index in 0..100 {
            let lock = service
                .workspace_lock(Path::new(&format!("/tmp/output-base-{index}")))
                .await;
            drop(lock);
        }
        let retained = service.workspace_lock(Path::new("/tmp/retained")).await;

        assert_eq!(service.workspace_locks.lock().await.len(), 1);
        assert!(Arc::strong_count(&retained) >= 1);
    }

    #[tokio::test]
    async fn rejects_runner_settings_that_would_panic_or_bypass_limits() {
        let root = tempdir().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let zero_concurrency = RunnerConfig {
            global_concurrency: 0,
            ..RunnerConfig::default()
        };
        assert!(InvocationService::new(store.clone(), zero_concurrency).is_err());

        let zero_timeout = RunnerConfig {
            maximum_timeout: Duration::ZERO,
            ..RunnerConfig::default()
        };
        assert!(InvocationService::new(store, zero_timeout).is_err());
    }

    #[test]
    fn accepts_absolute_bep_symlink_artifact_paths_for_later_containment_checks() {
        let artifact = bazel_mcp_types::Artifact {
            name: "test.xml".into(),
            kind: ArtifactKind::TestLog,
            uri: "/tmp/bazel-out/test.xml".into(),
            size_bytes: None,
            locally_available: true,
        };
        assert_eq!(
            local_artifact_path(&artifact),
            Some(PathBuf::from("/tmp/bazel-out/test.xml"))
        );
    }
}
