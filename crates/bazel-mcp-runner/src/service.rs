use std::{
    collections::{BTreeMap, HashMap},
    io,
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    process::ExitStatus,
    sync::{Arc, Weak},
    time::{Duration, Instant},
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use bazel_mcp_policy::{
    PolicyConfig, PolicyError, Redactor, effective_output_base, filtered_environment,
    resolve_bazel_executable, validate_arguments, validate_command, validate_query_arguments,
    validate_workspace,
};
use bazel_mcp_reducer::{
    Budget, StreamReductionOutput, finalize_diagnostics, normalize_terminal_text,
    parse_lcov_reader, parse_test_xml,
};
use bazel_mcp_store::{InvocationCompletion, InvocationPaths, Store, StoreError};
use bazel_mcp_types::{
    ArtifactKind, BazelCommand, CommandClass, DeferredFailure, DeferredFailureKind,
    DeferredResultRecord, DeferredResultView, DeferredRetrieval, DeferredTerminalState, Diagnostic,
    DiagnosticCategory, InvocationId, InvocationMetrics, InvocationRecord, InvocationRequest,
    InvocationState, Page, PageRequest, ResultDisposition, Severity, Termination, TestStatus,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::{Mutex, OwnedSemaphorePermit, Semaphore},
};
use tokio_util::sync::CancellationToken;

use crate::{
    cancel::{ProcessGroupGuard, terminate_child},
    capture,
    version::detect_bazel_version,
};

const COMPLETE_BEP_LOG_LIMIT: usize = 2 * 1024 * 1024;
const FALLBACK_LOG_LIMIT: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct RunnerConfig {
    pub policy: PolicyConfig,
    pub default_timeout: Duration,
    pub maximum_timeout: Duration,
    pub cancellation_interrupt_grace: Duration,
    pub cancellation_terminate_grace: Duration,
    pub global_concurrency: usize,
    pub output_user_root: Option<PathBuf>,
    pub isolated_bazel_server_idle_timeout: Duration,
    pub supported_bazel_major_versions: std::collections::BTreeSet<u32>,
    pub allow_unsupported_bazel_versions: bool,
    pub version_check_timeout: Duration,
    pub maximum_pending_invocations: usize,
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
            isolated_bazel_server_idle_timeout: Duration::from_secs(60),
            supported_bazel_major_versions: [7, 8, 9].into_iter().collect(),
            allow_unsupported_bazel_versions: false,
            version_check_timeout: Duration::from_secs(30),
            maximum_pending_invocations: 256,
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
    #[error("invocation queue is full (maximum {0} running or pending invocations)")]
    QueueFull(usize),
    #[error("invalid runner configuration: {0}")]
    InvalidConfiguration(&'static str),
    #[error("unsupported Bazel major version {detected}; supported major versions: {supported:?}")]
    UnsupportedBazelVersion { detected: u32, supported: Vec<u32> },
    #[error("Bazel compatibility check failed: {0}")]
    VersionCheck(String),
    #[error("invocation is already terminal: {0}")]
    AlreadyTerminal(InvocationId),
    #[error("requested log cursor is invalid or outside the file")]
    InvalidOffset,
    #[error("inspect response cannot fit the requested {0}-byte limit")]
    ResponseTooLarge(usize),
    #[error("invocation was cancelled before it was accepted")]
    CancelledBeforeAcceptance,
    #[error("wait for invocation {0} was cancelled")]
    WaitCancelled(InvocationId),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[derive(Clone)]
pub struct InvocationService {
    store: Store,
    config: RunnerConfig,
    redactor: Redactor,
    global: Arc<Semaphore>,
    pending: Arc<Semaphore>,
    workspace_locks: Arc<Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>>,
    live: Arc<Mutex<HashMap<InvocationId, CancellationToken>>>,
    version_cache: Arc<Mutex<HashMap<VersionCacheKey, u32>>>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct VersionCacheKey {
    executable: PathBuf,
    workspace: PathBuf,
    environment: Vec<(String, String)>,
}

struct PreparedSubmission {
    queued: InvocationRecord,
    paths: InvocationPaths,
    lock_key: PathBuf,
    executable: PathBuf,
    cancellation: CancellationToken,
    _pending_permit: OwnedSemaphorePermit,
    deferred: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InspectView {
    Summary,
    Metrics,
    Diagnostics,
    Tests,
    TestLog,
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
    pub state: Option<InvocationState>,
    pub command: Option<BazelCommand>,
    pub view: InspectView,
    pub cursor: Option<String>,
    pub filter: Option<String>,
    pub limit: u32,
    pub max_bytes: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct LogCursor {
    invocation_id: InvocationId,
    view: InspectView,
    next_record: u64,
    filter: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct EvidenceRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    text: String,
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

    fn decode_for(
        value: &str,
        invocation_id: InvocationId,
        view: InspectView,
        filter: Option<&str>,
    ) -> Result<Self, RunnerError> {
        let cursor = Self::decode(value)?;
        if cursor.invocation_id != invocation_id
            || cursor.view != view
            || cursor.filter.as_deref() != filter
        {
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
        if config.maximum_pending_invocations < config.global_concurrency {
            return Err(RunnerError::InvalidConfiguration(
                "maximum pending invocations must be at least global concurrency",
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
        if config.version_check_timeout.is_zero() {
            return Err(RunnerError::InvalidConfiguration(
                "version check timeout must be greater than zero",
            ));
        }
        if config.isolated_bazel_server_idle_timeout.is_zero() {
            return Err(RunnerError::InvalidConfiguration(
                "isolated Bazel server idle timeout must be greater than zero",
            ));
        }
        if !config.allow_unsupported_bazel_versions
            && config.supported_bazel_major_versions.is_empty()
        {
            return Err(RunnerError::InvalidConfiguration(
                "supported Bazel major versions must not be empty",
            ));
        }
        let redactor = Redactor::new(&config.policy.redaction_patterns)?;
        Ok(Self {
            store,
            global: Arc::new(Semaphore::new(config.global_concurrency)),
            pending: Arc::new(Semaphore::new(config.maximum_pending_invocations)),
            config,
            redactor,
            workspace_locks: Arc::new(Mutex::new(HashMap::new())),
            live: Arc::new(Mutex::new(HashMap::new())),
            version_cache: Arc::new(Mutex::new(HashMap::new())),
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
        request: InvocationRequest,
        cancellation: CancellationToken,
    ) -> Result<InvocationRecord, RunnerError> {
        let prepared = self
            .prepare_submission(request, ResultDisposition::Attached, cancellation)
            .await?;
        let id = prepared.queued.request.id;
        let result = self.run_prepared(prepared).await;
        self.live.lock().await.remove(&id);
        result
    }

    /// Durably accept an invocation and execute it independently of the caller.
    pub async fn submit(
        &self,
        request: InvocationRequest,
        disposition: ResultDisposition,
    ) -> Result<InvocationId, RunnerError> {
        let prepared = self
            .prepare_submission(request, disposition, CancellationToken::new())
            .await?;
        let id = prepared.queued.request.id;
        let deferred = prepared.deferred;
        let worker_service = self.clone();
        let worker = tokio::spawn(async move { worker_service.run_prepared(prepared).await });
        let supervisor = self.clone();
        tokio::spawn(async move {
            let error = match worker.await {
                Ok(Ok(record)) => {
                    if deferred {
                        tracing::info!(
                            target: "bazel_mcp::metrics",
                            metric = "deferred_invocations_terminal_total",
                            increment = 1_u64,
                            state = ?record.state,
                            "deferred invocation reached a terminal state"
                        );
                    }
                    None
                }
                Ok(Err(error)) => Some(error),
                Err(error) => Some(RunnerError::Join(error)),
            };
            if let Some(error) = error {
                tracing::warn!(invocation_id = %id, %error, "accepted Bazel invocation worker failed");
                supervisor
                    .materialize_worker_failure(id, deferred, &error)
                    .await;
                if deferred {
                    tracing::info!(
                        target: "bazel_mcp::metrics",
                        metric = "deferred_invocations_terminal_total",
                        increment = 1_u64,
                        state = "accepted_execution_error",
                        "deferred invocation failure was materialized"
                    );
                }
            }
            supervisor.live.lock().await.remove(&id);
        });
        Ok(id)
    }

    /// Wait for completion without coupling cancellation of this wait to Bazel.
    pub async fn wait(
        &self,
        id: InvocationId,
        cancellation: CancellationToken,
    ) -> Result<InvocationRecord, RunnerError> {
        loop {
            let record = self.store.get_invocation(id).await?;
            if record.state.is_terminal() {
                return Ok(record);
            }
            tokio::select! {
                () = cancellation.cancelled() => return Err(RunnerError::WaitCancelled(id)),
                () = tokio::time::sleep(Duration::from_millis(25)) => {}
            }
        }
    }

    pub async fn deferred_result(
        &self,
        id: InvocationId,
        retrieval: DeferredRetrieval,
    ) -> Result<DeferredResultView, RunnerError> {
        self.store
            .get_deferred_result(id, retrieval, bazel_mcp_types::unix_timestamp_ms())
            .await
            .map_err(Into::into)
    }

    pub async fn list_deferred_results(
        &self,
        retrieval: DeferredRetrieval,
        page: PageRequest,
    ) -> Result<Page<DeferredResultView>, RunnerError> {
        self.store
            .list_deferred_results(retrieval, bazel_mcp_types::unix_timestamp_ms(), page)
            .await
            .map_err(Into::into)
    }

    pub async fn record_deferred_cancellation(&self, id: InvocationId) -> Result<(), RunnerError> {
        self.store
            .record_deferred_cancellation(id, bazel_mcp_types::unix_timestamp_ms())
            .await?;
        Ok(())
    }

    pub async fn set_deferred_cancelled(&self, id: InvocationId) -> Result<(), RunnerError> {
        self.store
            .set_deferred_terminal_override(
                id,
                DeferredTerminalState::Cancelled,
                bazel_mcp_types::unix_timestamp_ms(),
            )
            .await?;
        Ok(())
    }

    pub async fn extend_deferred_expiry(
        &self,
        id: InvocationId,
        minimum_expires_at_ms: i64,
    ) -> Result<(), RunnerError> {
        self.store
            .extend_deferred_expiry(
                id,
                minimum_expires_at_ms,
                bazel_mcp_types::unix_timestamp_ms(),
            )
            .await?;
        Ok(())
    }

    async fn prepare_submission(
        &self,
        mut request: InvocationRequest,
        disposition: ResultDisposition,
        cancellation: CancellationToken,
    ) -> Result<PreparedSubmission, RunnerError> {
        if cancellation.is_cancelled() {
            return Err(RunnerError::CancelledBeforeAcceptance);
        }
        validate_command(&self.config.policy, &request.command)?;
        validate_arguments(&request.startup_arguments)?;
        validate_arguments(&request.arguments)?;
        validate_query_arguments(&request.command, &request.arguments)?;
        let pending_permit = self
            .pending
            .clone()
            .try_acquire_owned()
            .map_err(|_| RunnerError::QueueFull(self.config.maximum_pending_invocations))?;
        let workspace = validate_workspace(&request.workspace, &self.config.policy.allowed_roots)?;
        let lock_key = effective_output_base(&workspace, &request.startup_arguments)?
            .unwrap_or_else(|| workspace.clone());
        let executable = resolve_bazel_executable(&workspace, &self.config.policy)?;
        self.validate_bazel_version(&executable, &workspace).await?;
        if cancellation.is_cancelled() {
            return Err(RunnerError::CancelledBeforeAcceptance);
        }
        request.workspace = workspace.clone();
        let id = request.id;
        let queued = InvocationRecord::queued(request);
        let stored = InvocationRecord::queued(self.redacted_request(&queued.request));
        let deferred = match disposition {
            ResultDisposition::Attached => None,
            ResultDisposition::Deferred {
                retrieval,
                expires_at_ms,
            } => {
                let now_ms = bazel_mcp_types::unix_timestamp_ms();
                Some(DeferredResultRecord::new(
                    id,
                    retrieval,
                    now_ms,
                    expires_at_ms,
                ))
            }
        };
        let paths = self
            .store
            .create_invocation_with_deferred(&stored, deferred.as_ref())
            .await?;
        self.live.lock().await.insert(id, cancellation.clone());

        Ok(PreparedSubmission {
            queued,
            paths,
            lock_key,
            executable,
            cancellation,
            _pending_permit: pending_permit,
            deferred: deferred.is_some(),
        })
    }

    async fn run_prepared(
        &self,
        prepared: PreparedSubmission,
    ) -> Result<InvocationRecord, RunnerError> {
        let PreparedSubmission {
            queued,
            paths,
            lock_key,
            executable,
            cancellation,
            _pending_permit,
            deferred: _,
        } = prepared;
        let id = queued.request.id;
        async {
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
            if let Err(error) = self
                .store
                .transition(id, InvocationState::Starting, None, None)
                .await
            {
                let message = self.redactor.redact_bounded(
                    &format!("could not record Bazel starting state: {error}"),
                    1_000,
                );
                tracing::warn!(invocation_id = %id, %message);
                if self
                    .store
                    .get_invocation(id)
                    .await
                    .is_ok_and(|record| !record.state.is_terminal())
                {
                    let _ = self
                        .store
                        .transition(
                            id,
                            InvocationState::Failed,
                            Some(Termination::SpawnFailure {
                                message: message.clone(),
                            }),
                            Some(bazel_mcp_types::InvocationSummary {
                                success: false,
                                headline: format!("Could not start Bazel: {message}"),
                                truncated: true,
                                ..Default::default()
                            }),
                        )
                        .await;
                }
                if let Ok(record) = self.store.get_invocation(id).await
                    && record.state.is_terminal()
                    && record.summary.is_some()
                {
                    return Ok(record);
                }
                return Err(error.into());
            }
            let result = self
                .execute(&queued, &paths, &executable, cancellation.clone())
                .await;
            drop(workspace_guard);
            drop(permit);
            result
        }
        .await
    }

    async fn materialize_worker_failure(
        &self,
        id: InvocationId,
        deferred: bool,
        error: &RunnerError,
    ) {
        let message = self.redactor.redact_bounded(
            &format!("accepted invocation worker failed: {error}"),
            1_000,
        );
        if deferred {
            let _ = self
                .store
                .persist_deferred_failure(
                    id,
                    &DeferredFailure {
                        kind: DeferredFailureKind::Execution,
                        redacted_message: message.clone(),
                    },
                    bazel_mcp_types::unix_timestamp_ms(),
                )
                .await;
        }
        if self
            .store
            .get_invocation(id)
            .await
            .is_ok_and(|record| !record.state.is_terminal())
        {
            let _ = self
                .store
                .transition(
                    id,
                    InvocationState::Failed,
                    Some(Termination::SpawnFailure {
                        message: message.clone(),
                    }),
                    Some(bazel_mcp_types::InvocationSummary {
                        success: false,
                        headline: "Accepted Bazel invocation could not be executed".to_owned(),
                        truncated: true,
                        ..Default::default()
                    }),
                )
                .await;
        }
    }

    async fn validate_bazel_version(
        &self,
        executable: &Path,
        workspace: &Path,
    ) -> Result<(), RunnerError> {
        if self.config.allow_unsupported_bazel_versions {
            return Ok(());
        }
        let environment = filtered_environment(&self.config.policy);
        let key = VersionCacheKey {
            executable: executable.to_owned(),
            workspace: workspace.to_owned(),
            environment: environment
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect(),
        };
        let mut version_cache = self.version_cache.lock().await;
        let major = if let Some(major) = version_cache.get(&key).copied() {
            major
        } else {
            let version = detect_bazel_version(
                executable,
                workspace,
                &environment,
                self.config.version_check_timeout,
                self.config.cancellation_interrupt_grace,
                self.config.cancellation_terminate_grace,
            )
            .await
            .map_err(|error| RunnerError::VersionCheck(error.to_string()))?;
            version_cache.insert(key, version.major);
            version.major
        };
        drop(version_cache);
        if self.config.supported_bazel_major_versions.contains(&major) {
            Ok(())
        } else {
            Err(RunnerError::UnsupportedBazelVersion {
                detected: major,
                supported: self
                    .config
                    .supported_bazel_major_versions
                    .iter()
                    .copied()
                    .collect(),
            })
        }
    }

    pub async fn cancel(&self, id: InvocationId) -> Result<CancelResult, RunnerError> {
        self.cancel_with_reason(id, None).await
    }

    /// Request cancellation for every invocation owned by this server process.
    /// Used during graceful transport and operating-system shutdown.
    pub async fn cancel_all_active(&self) -> usize {
        let cancellations: Vec<_> = self.live.lock().await.values().cloned().collect();
        let count = cancellations.len();
        for cancellation in cancellations {
            cancellation.cancel();
        }
        count
    }

    pub async fn active_invocation_count(&self) -> usize {
        self.live.lock().await.len()
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
            let mut limit = request.limit.clamp(1, 100);
            loop {
                let page = self
                    .store
                    .list_invocations(
                        request.workspace.as_deref(),
                        request.state,
                        request.command.as_ref(),
                        PageRequest {
                            cursor: request.cursor.clone(),
                            limit,
                            max_bytes: None,
                        },
                    )
                    .await?;
                let result = InspectResult {
                    invocation_id: None,
                    view: request.view,
                    items: serde_json::Value::Array(
                        page.items.iter().map(invocation_ledger_row).collect(),
                    ),
                    total_count: None,
                    filtered_count: None,
                    next_cursor: page.next_cursor,
                    truncated: page.truncated,
                };
                if serialized_len(&result)? <= request.max_bytes || limit == 1 {
                    return enforce_inspect_budget(result, request.max_bytes);
                }
                limit = (limit / 2).max(1);
            }
        }

        let id = request.invocation_id.ok_or(StoreError::InvalidCursor)?;
        let record = self.store.get_invocation(id).await?;
        let page_request = PageRequest {
            cursor: request.cursor.clone(),
            limit: request.limit.clamp(1, 100),
            max_bytes: Some(request.max_bytes.saturating_sub(512)),
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
            InspectView::Metrics => {
                let items = vec![serde_json::json!({
                    "state": record.state,
                    "requested_at_ms": record.request.requested_at_ms,
                    "started_at_ms": record.started_at_ms,
                    "finished_at_ms": record.finished_at_ms,
                    "termination": record.termination,
                    "metrics": record.metrics,
                })];
                (serde_json::to_value(items)?, Some(1), Some(1), None, false)
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
                let redactor = self.redactor.clone();
                let (page, total, filtered) = self
                    .store
                    .page_query_rows_mapped(
                        id,
                        request.filter.as_deref(),
                        page_request,
                        move |value| redactor.redact_bounded(value, 4 * 1024),
                    )
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
                                max_bytes: Some(request.max_bytes.saturating_sub(512)),
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
                let (items, truncated, next_cursor) = self
                    .read_evidence_page(&paths.evidence, &paths, &request)
                    .await?;
                (
                    serde_json::to_value(items)?,
                    None,
                    None,
                    next_cursor,
                    truncated,
                )
            }
            InspectView::TestLog => {
                let (items, truncated, next_cursor) = if paths.test_log_evidence.exists() {
                    self.read_evidence_page(&paths.test_log_evidence, &paths, &request)
                        .await?
                } else {
                    self.read_test_log_unavailable_page(id, &request).await?
                };
                (
                    serde_json::to_value(items)?,
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
        let (stdout, stderr) = match capture::open_stdio(paths).await {
            Ok(streams) => streams,
            Err(error) => {
                let message = self.redactor.redact_bounded(
                    &format!("could not create Bazel evidence files: {error}"),
                    1_000,
                );
                let summary = bazel_mcp_types::InvocationSummary {
                    success: false,
                    headline: format!("Could not prepare Bazel invocation: {message}"),
                    truncated: true,
                    ..Default::default()
                };
                return self
                    .store
                    .transition(
                        queued.request.id,
                        InvocationState::Failed,
                        Some(Termination::SpawnFailure {
                            message: message.clone(),
                        }),
                        Some(summary),
                    )
                    .await
                    .map_err(Into::into);
            }
        };
        let mut command = Command::new(executable);
        command
            .current_dir(&queued.request.workspace)
            .env_clear()
            .envs(filtered_environment(&self.config.policy));
        if let Some(output_user_root) = &self.config.output_user_root {
            command
                .arg(format!("--output_user_root={}", output_user_root.display()))
                .arg(format!(
                    "--max_idle_secs={}",
                    self.config.isolated_bazel_server_idle_timeout.as_secs()
                ));
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
            if matches!(
                queued.request.command,
                BazelCommand::Test | BazelCommand::Coverage
            ) {
                command.args(["--test_output=errors", "--test_summary=none"]);
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
        let mut process_group = ProcessGroupGuard::for_child(&child);
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
            let workspace = queued.request.workspace.to_string_lossy();
            let message = self.redactor.redact_bounded(
                &error.to_string().replace(workspace.as_ref(), "<workspace>"),
                1_000,
            );
            tracing::warn!(
                invocation_id = %queued.request.id,
                error = %message,
                "could not record running Bazel invocation"
            );
            if self
                .store
                .get_invocation(queued.request.id)
                .await
                .is_ok_and(|record| !record.state.is_terminal())
            {
                let summary = bazel_mcp_types::InvocationSummary {
                    success: false,
                    headline: format!("Could not record Bazel execution state: {message}"),
                    truncated: true,
                    inspect_hint: Some("log".to_owned()),
                    ..Default::default()
                };
                let _ = self
                    .store
                    .transition(
                        queued.request.id,
                        InvocationState::Failed,
                        Some(Termination::Interrupted),
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
        process_group.disarm();
        let bazel_wall_ms = duration_millis(started.elapsed());
        let postprocess: Result<InvocationRecord, RunnerError> = async {
            let reduction_started = Instant::now();
            let (query_row_count, query_sample) =
                if queued.request.command.class() == CommandClass::Query {
                    let redactor = self.redactor.clone();
                    let (page, total, _) = self
                        .store
                        .page_query_rows_mapped(
                            queued.request.id,
                            None,
                            PageRequest {
                                cursor: None,
                                limit: 3,
                                max_bytes: None,
                            },
                            move |value| redactor.redact_bounded(value, 4 * 1024),
                        )
                        .await?;
                    (total, page.items)
                } else {
                    (0, Vec::new())
                };
            let (bep, bep_outcome) = capture::reduce_bep(paths.bep.clone()).await?;
            if let Some(error) = &bep_outcome.terminal_error {
                tracing::warn!(invocation_id = %queued.request.id, %error, "partially decoded BEP");
            }
            // Complete structured evidence needs only a small diagnostic tail;
            // partial or missing BEP retains a larger bounded fallback.
            let log_limit = if bep_outcome.event_count > 0 && bep_outcome.terminal_error.is_none() {
                COMPLETE_BEP_LOG_LIMIT
            } else {
                FALLBACK_LOG_LIMIT
            };
            let stdout = capture::read_bounded_tail(&paths.stdout, log_limit).await?;
            let stderr = capture::read_bounded_tail(&paths.stderr, log_limit).await?;
            let exit_code = status.as_ref().and_then(ExitStatus::code);
            self.persist_failure_evidence(
                paths,
                &queued.request.workspace,
                &queued.request.command,
                exit_code != Some(0),
                &stdout,
                &stderr,
            )
            .await?;
            let reduced = catch_unwind(AssertUnwindSafe(|| {
                bep.finish(
                    &stdout,
                    &stderr,
                    exit_code,
                    bazel_wall_ms,
                    // Enrichment can add higher-value failed-test evidence.
                    // Apply the public result budget only after that evidence
                    // is present so ranking and aggregation precede limits.
                    Budget {
                        max_bytes: usize::MAX,
                        max_items: usize::MAX,
                    },
                )
            }));
            let StreamReductionOutput {
                mut summary,
                mut artifacts,
                canonical_arguments,
            } = reduced.unwrap_or_else(|_| {
                tracing::warn!(
                    invocation_id = %queued.request.id,
                    "streaming BEP reducer panicked; using bounded fallback summary"
                );
                StreamReductionOutput {
                    summary: fallback_summary(exit_code, bazel_wall_ms, &stderr, &stdout),
                    artifacts: Vec::new(),
                    canonical_arguments: None,
                }
            });
            let canonical_arguments = canonical_arguments.map(|mut arguments| {
                let workspace = queued.request.workspace.to_string_lossy();
                for argument in &mut arguments {
                    *argument = self.redactor.redact_bounded(
                        &argument.replace(workspace.as_ref(), "<workspace>"),
                        64 * 1024,
                    );
                }
                arguments
            });
            for artifact in &mut artifacts {
                artifact.name = self.redactor.redact_bounded(&artifact.name, 1_000);
                artifact.uri = self.redactor.redact_bounded(&artifact.uri, 1_000);
            }
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
            self.enrich_tests(paths, &queued.request.workspace, &mut summary, &artifacts)
                .await;
            finalize_diagnostics(&mut summary, Budget::result_default());
            if queued.request.command == BazelCommand::Coverage {
                summary.coverage = self
                    .load_coverage(&queued.request.workspace, &artifacts)
                    .await;
            }
            self.sanitize_summary(queued.request.id, &queued.request.workspace, &mut summary);
            let metrics = InvocationMetrics {
                raw_output_bytes: capture::file_size(&paths.stdout)
                    .await
                    .saturating_add(capture::file_size(&paths.stderr).await),
                bep_bytes: capture::file_size(&paths.bep).await,
                bep_events: u64::try_from(bep_outcome.event_count).unwrap_or(u64::MAX),
                queue_ms,
                bazel_wall_ms,
                reduction_ms: duration_millis(reduction_started.elapsed()),
                ..Default::default()
            };
            self.store
                .finish_invocation(
                    queued.request.id,
                    InvocationCompletion {
                        state,
                        termination: termination.clone(),
                        summary,
                        metrics,
                        canonical_arguments,
                        artifacts,
                    },
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
        _id: InvocationId,
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
        for test in &mut summary.tests {
            test.label = sanitize(&test.label, 1_000);
            for case in &mut test.cases {
                case.name = sanitize(&case.name, 512);
                case.message = case
                    .message
                    .as_deref()
                    .map(|message| sanitize(message, 1_000));
            }
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
            Ok(record) => Ok(record),
            Err(StoreError::State(_)) => {
                let record = self.store.get_invocation(id).await?;
                if record.state.is_terminal() {
                    Ok(record)
                } else {
                    Err(RunnerError::Store(StoreError::State(
                        bazel_mcp_types::StateTransitionError {
                            current: record.state,
                            next: InvocationState::Cancelled,
                        },
                    )))
                }
            }
            Err(error) => Err(error.into()),
        }
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
        paths: &InvocationPaths,
        workspace: &Path,
        summary: &mut bazel_mcp_types::InvocationSummary,
        artifacts: &[bazel_mcp_types::Artifact],
    ) {
        if !summary
            .tests
            .iter()
            .any(|test| test.status != TestStatus::Passed)
        {
            return;
        }

        let raw_temporary = paths.test_logs_raw.with_extension("tmp");
        let evidence_temporary = paths.test_log_evidence.with_extension("tmp");
        if tokio::fs::try_exists(&paths.test_logs_raw)
            .await
            .unwrap_or(false)
            || tokio::fs::try_exists(&paths.test_log_evidence)
                .await
                .unwrap_or(false)
        {
            for test in summary
                .tests
                .iter_mut()
                .filter(|test| test.status != TestStatus::Passed)
            {
                test.test_log_available = false;
                test.test_log_unavailable_reason =
                    Some("test_log_snapshot_already_exists".to_owned());
            }
            return;
        }

        let raw = tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&raw_temporary)
            .await;
        let evidence = tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&evidence_temporary)
            .await;
        let (Ok(mut raw), Ok(mut evidence)) = (raw, evidence) else {
            let _ = tokio::fs::remove_file(&raw_temporary).await;
            let _ = tokio::fs::remove_file(&evidence_temporary).await;
            set_test_log_unavailable(summary, "test_log_snapshot_failed");
            return;
        };

        let workspace_text = workspace.to_string_lossy();
        let any_remote_test_log = artifacts.iter().any(|artifact| {
            artifact.kind == ArtifactKind::TestLog
                && artifact.name.ends_with("test.log")
                && !artifact.locally_available
        });
        let mut diagnostics = Vec::new();
        let mut copied_any = false;
        for test in summary
            .tests
            .iter_mut()
            .filter(|test| test.status != TestStatus::Passed)
        {
            if let Some(xml) = artifacts.iter().find(|artifact| {
                artifact.kind == ArtifactKind::TestLog
                    && artifact.name.ends_with("test.xml")
                    && artifact_matches_test(artifact, &test.label)
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
                            case.message =
                                case.message.map(|message| bounded_text(&message, 1_000));
                            case
                        })
                        .collect();
                }
            }

            let matching_logs = artifacts.iter().filter(|artifact| {
                artifact.kind == ArtifactKind::TestLog
                    && artifact.name.ends_with("test.log")
                    && artifact_matches_test(artifact, &test.label)
            });
            let mut saw_artifact = false;
            let mut copied_for_test = false;
            let mut saw_remote = false;
            let mut actionable_excerpt = None::<String>;
            for log in matching_logs {
                saw_artifact = true;
                if !log.locally_available {
                    saw_remote = true;
                    continue;
                }
                let Some(path) = self.validated_artifact_path(workspace, log).await else {
                    continue;
                };
                let Ok(file) = tokio::fs::File::open(&path).await else {
                    continue;
                };
                let marker = format!("\n===== {} :: {} =====\n", test.label, log.name);
                if raw.write_all(marker.as_bytes()).await.is_err() {
                    continue;
                }
                let mut reader = BufReader::new(file);
                let mut line = Vec::new();
                let mut complete = true;
                loop {
                    line.clear();
                    let read = match reader.read_until(b'\n', &mut line).await {
                        Ok(read) => read,
                        Err(_) => {
                            complete = false;
                            break;
                        }
                    };
                    if read == 0 {
                        break;
                    }
                    if raw.write_all(&line).await.is_err() {
                        complete = false;
                        break;
                    }
                    for visible_line in normalize_terminal_text(&line).lines() {
                        let visible_line = visible_line.trim();
                        if visible_line.is_empty() {
                            continue;
                        }
                        let text = self.redactor.redact_bounded(
                            &visible_line.replace(workspace_text.as_ref(), "<workspace>"),
                            4 * 1024,
                        );
                        if is_actionable_evidence(&text) {
                            actionable_excerpt = Some(bounded_text(&text, 1_000));
                        }
                        let record = EvidenceRecord {
                            label: Some(test.label.clone()),
                            text: format!("[{}] {text}", test.label),
                        };
                        let Ok(mut encoded) = serde_json::to_vec(&record) else {
                            complete = false;
                            break;
                        };
                        encoded.push(b'\n');
                        if evidence.write_all(&encoded).await.is_err() {
                            complete = false;
                            break;
                        }
                    }
                    if !complete {
                        break;
                    }
                }
                if complete {
                    copied_for_test = true;
                    copied_any = true;
                }
            }
            if copied_for_test {
                test.test_log_available = true;
                test.test_log_unavailable_reason = None;
                if let Some(message) = actionable_excerpt {
                    diagnostics.push(Diagnostic {
                        severity: Severity::Error,
                        category: DiagnosticCategory::Compilation,
                        message,
                        location: None,
                        target: Some(test.label.clone()),
                        action: None,
                        repetition_count: 1,
                    });
                }
            } else {
                test.test_log_available = false;
                test.test_log_unavailable_reason = Some(
                    if saw_remote || (!saw_artifact && any_remote_test_log) {
                        "remote_test_log_unavailable"
                    } else if saw_artifact {
                        "test_log_outside_allowed_roots_or_unreadable"
                    } else {
                        "test_log_not_found"
                    }
                    .to_owned(),
                );
            }
        }

        let flushed = raw.flush().await.is_ok() && evidence.flush().await.is_ok();
        drop(raw);
        drop(evidence);
        let committed = if copied_any {
            flushed
                && set_private_file(&raw_temporary).await.is_ok()
                && set_private_file(&evidence_temporary).await.is_ok()
                && tokio::fs::rename(&raw_temporary, &paths.test_logs_raw)
                    .await
                    .is_ok()
                && tokio::fs::rename(&evidence_temporary, &paths.test_log_evidence)
                    .await
                    .is_ok()
        } else {
            false
        };
        if committed {
            summary.diagnostics.extend(diagnostics);
        } else {
            let _ = tokio::fs::remove_file(&raw_temporary).await;
            let _ = tokio::fs::remove_file(&evidence_temporary).await;
            if copied_any {
                set_test_log_unavailable(summary, "test_log_snapshot_failed");
            }
        }
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
        let canonical = tokio::fs::canonicalize(&path).await.ok()?;
        let in_workspace = canonical.starts_with(workspace);
        let in_output_root = if let Some(root) = &self.config.output_user_root {
            tokio::fs::canonicalize(root)
                .await
                .is_ok_and(|root| canonical.starts_with(root))
        } else {
            false
        };
        let in_bazel_testlogs = if artifact.kind == ArtifactKind::TestLog {
            match bazel_test_log_output_base(&path).zip(bazel_test_log_output_base(&canonical)) {
                Some((lexical_root, canonical_root)) => tokio::fs::canonicalize(lexical_root)
                    .await
                    .is_ok_and(|lexical_root| {
                        lexical_root == canonical_root && canonical.starts_with(&lexical_root)
                    }),
                None => false,
            }
        } else {
            false
        };
        (in_workspace || in_output_root || in_bazel_testlogs).then_some(canonical)
    }

    async fn persist_failure_evidence(
        &self,
        paths: &InvocationPaths,
        workspace: &Path,
        command: &BazelCommand,
        failed: bool,
        stdout: &[u8],
        stderr: &[u8],
    ) -> Result<(), RunnerError> {
        let records = failure_evidence_records(command, failed, stdout, stderr);
        let workspace = workspace.to_string_lossy();
        let mut bytes = Vec::new();
        for mut record in records {
            record.text = self.redactor.redact_bounded(
                &record.text.replace(workspace.as_ref(), "<workspace>"),
                4 * 1024,
            );
            serde_json::to_writer(&mut bytes, &record)?;
            bytes.push(b'\n');
        }
        write_private_atomic(&paths.evidence, &bytes).await?;
        Ok(())
    }

    async fn read_evidence_page(
        &self,
        path: &Path,
        paths: &InvocationPaths,
        request: &InspectRequest,
    ) -> Result<(Vec<String>, bool, Option<String>), RunnerError> {
        let invocation_id = request.invocation_id.ok_or(StoreError::InvalidCursor)?;
        let start = request
            .cursor
            .as_deref()
            .map(|value| {
                LogCursor::decode_for(
                    value,
                    invocation_id,
                    request.view,
                    request.filter.as_deref(),
                )
            })
            .transpose()?
            .map_or(0, |cursor| cursor.next_record);
        let file = match tokio::fs::File::open(path).await {
            Ok(file) => file,
            Err(error)
                if error.kind() == io::ErrorKind::NotFound && request.view == InspectView::Log =>
            {
                let stdout = capture::read_bounded_tail(&paths.stdout, 1024 * 1024).await?;
                let stderr = capture::read_bounded_tail(&paths.stderr, 1024 * 1024).await?;
                let records = failure_evidence_records(
                    &BazelCommand::Custom("retained".to_owned()),
                    true,
                    &stdout,
                    &stderr,
                );
                return page_evidence_records(records.into_iter(), start, request, invocation_id);
            }
            Err(error) => return Err(error.into()),
        };
        let mut lines = BufReader::new(file).lines();
        let mut records = Vec::new();
        while let Some(line) = lines.next_line().await? {
            records.push(serde_json::from_str::<EvidenceRecord>(&line)?);
        }
        page_evidence_records(records.into_iter(), start, request, invocation_id)
    }

    async fn read_test_log_unavailable_page(
        &self,
        invocation_id: InvocationId,
        request: &InspectRequest,
    ) -> Result<(Vec<String>, bool, Option<String>), RunnerError> {
        let start = request
            .cursor
            .as_deref()
            .map(|value| {
                LogCursor::decode_for(
                    value,
                    invocation_id,
                    request.view,
                    request.filter.as_deref(),
                )
            })
            .transpose()?
            .map_or(0, |cursor| cursor.next_record);
        let maximum_bytes = request.max_bytes.saturating_sub(512).max(1);
        let maximum_items = usize::try_from(request.limit.clamp(1, 100)).unwrap_or(100);
        let filter = request.filter.as_deref().map(str::to_ascii_lowercase);
        let mut items = Vec::new();
        let mut used_bytes = 2_usize;
        let mut reason_index = 0_u64;
        let mut storage_cursor = None;

        loop {
            let (page, _, _) = self
                .store
                .page_tests(
                    invocation_id,
                    None,
                    PageRequest {
                        cursor: storage_cursor,
                        limit: 100,
                        max_bytes: Some(128 * 1024),
                    },
                )
                .await?;
            for test in page.items {
                let Some(reason) = test.test_log_unavailable_reason else {
                    continue;
                };
                let index = reason_index;
                reason_index = reason_index.saturating_add(1);
                if index < start {
                    continue;
                }
                let text = self
                    .redactor
                    .redact_bounded(&format!("{}: {reason}", test.label), 4 * 1024);
                if !filter
                    .as_ref()
                    .is_none_or(|filter| text.to_ascii_lowercase().contains(filter))
                {
                    continue;
                }
                let item_bytes = serde_json::to_vec(&text)?.len();
                let separator = usize::from(!items.is_empty());
                if items.len() == maximum_items
                    || (!items.is_empty()
                        && used_bytes
                            .saturating_add(separator)
                            .saturating_add(item_bytes)
                            > maximum_bytes)
                {
                    let next_cursor = LogCursor {
                        invocation_id,
                        view: request.view,
                        next_record: index,
                        filter: request.filter.clone(),
                    }
                    .encode()?;
                    return Ok((items, true, Some(next_cursor)));
                }
                used_bytes = used_bytes
                    .saturating_add(separator)
                    .saturating_add(item_bytes);
                items.push(text);
            }
            if !page.truncated {
                return Ok((items, false, None));
            }
            storage_cursor = page.next_cursor;
        }
    }
}

fn page_evidence_records(
    records: impl Iterator<Item = EvidenceRecord>,
    start: u64,
    request: &InspectRequest,
    invocation_id: InvocationId,
) -> Result<(Vec<String>, bool, Option<String>), RunnerError> {
    let maximum_bytes = request.max_bytes.saturating_sub(512).max(1);
    let maximum_items = usize::try_from(request.limit.clamp(1, 100)).unwrap_or(100);
    let filter = request.filter.as_deref().map(str::to_ascii_lowercase);
    let mut items = Vec::new();
    let mut used_bytes = 2_usize;
    let mut next_record = start;
    let mut truncated = false;
    for (index, record) in records.enumerate() {
        let index = u64::try_from(index).unwrap_or(u64::MAX);
        if index < start {
            continue;
        }
        let matches = filter.as_ref().is_none_or(|filter| {
            record.text.to_ascii_lowercase().contains(filter)
                || record
                    .label
                    .as_deref()
                    .is_some_and(|label| label.to_ascii_lowercase().contains(filter))
        });
        if !matches {
            next_record = index.saturating_add(1);
            continue;
        }
        let item_bytes = serde_json::to_vec(&record.text)?.len();
        let separator = usize::from(!items.is_empty());
        if items.len() == maximum_items
            || (!items.is_empty()
                && used_bytes
                    .saturating_add(separator)
                    .saturating_add(item_bytes)
                    > maximum_bytes)
        {
            next_record = index;
            truncated = true;
            break;
        }
        used_bytes = used_bytes
            .saturating_add(separator)
            .saturating_add(item_bytes);
        items.push(record.text);
        next_record = index.saturating_add(1);
    }
    let next_cursor = truncated
        .then_some(LogCursor {
            invocation_id,
            view: request.view,
            next_record,
            filter: request.filter.clone(),
        })
        .map(|cursor| cursor.encode())
        .transpose()?;
    Ok((items, truncated, next_cursor))
}

fn failure_evidence_records(
    command: &BazelCommand,
    failed: bool,
    stdout: &[u8],
    stderr: &[u8],
) -> Vec<EvidenceRecord> {
    let test_like = matches!(command, BazelCommand::Test | BazelCommand::Coverage);
    let streams = if (failed && test_like) || (!failed && !stdout.is_empty()) {
        [stdout, stderr]
    } else {
        [stderr, stdout]
    };
    let mut positions = BTreeMap::<String, usize>::new();
    let mut lines = Vec::<(String, u32)>::new();
    for stream in streams {
        for line in normalize_terminal_text(stream).lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let line = bounded_text(line, 4 * 1024);
            if let Some(index) = positions.get(&line).copied() {
                lines[index].1 = lines[index].1.saturating_add(1);
            } else {
                positions.insert(line.clone(), lines.len());
                lines.push((line, 1));
            }
        }
    }
    let mut actionable = Vec::new();
    let mut fallback = Vec::new();
    for (line, count) in lines {
        let record = EvidenceRecord {
            label: None,
            text: if count > 1 {
                format!("{line} [repeated {count} times]")
            } else {
                line
            },
        };
        if is_actionable_evidence(&record.text) {
            actionable.push(record);
        } else {
            fallback.push(record);
        }
    }
    let fallback_start = fallback.len().saturating_sub(2_000);
    actionable.truncate(1_000);
    actionable.extend(fallback.into_iter().skip(fallback_start));
    actionable.truncate(3_000);
    actionable
}

fn is_actionable_evidence(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("error:")
        || lower.starts_with("error ")
        || lower.contains("failed:")
        || lower.contains("failure")
        || lower.contains("fatal:")
        || lower.contains("no such target")
        || lower.contains("no such package")
        || lower.contains("undefined reference")
        || lower.contains("root_cause")
}

async fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<(), io::Error> {
    let temporary = path.with_extension("tmp");
    let mut file = tokio::fs::File::create(&temporary).await?;
    file.write_all(bytes).await?;
    file.flush().await?;
    drop(file);
    set_private_file(&temporary).await?;
    tokio::fs::rename(temporary, path).await
}

#[cfg(unix)]
async fn set_private_file(path: &Path) -> Result<(), io::Error> {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await
}

#[cfg(not(unix))]
async fn set_private_file(_path: &Path) -> Result<(), io::Error> {
    Ok(())
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

fn artifact_matches_test(artifact: &bazel_mcp_types::Artifact, label: &str) -> bool {
    let label = label.rsplit_once("//").map_or(label, |(_, label)| label);
    let (package, target) = label
        .split_once(':')
        .map_or(("", label.trim_start_matches("//")), |(package, target)| {
            (package.trim_start_matches("//"), target)
        });
    let fragment = if package.is_empty() {
        format!("/testlogs/{target}/")
    } else {
        format!("/testlogs/{package}/{target}/")
    };
    artifact.uri.replace('\\', "/").contains(&fragment)
}

fn bazel_test_log_output_base(path: &Path) -> Option<PathBuf> {
    let components = path.components().collect::<Vec<_>>();
    let execroot = components
        .iter()
        .position(|component| component.as_os_str() == "execroot")?;
    let bazel_out = components
        .iter()
        .enumerate()
        .skip(execroot + 1)
        .find(|(_, component)| component.as_os_str() == "bazel-out")?
        .0;
    components
        .iter()
        .enumerate()
        .skip(bazel_out + 1)
        .find(|(_, component)| component.as_os_str() == "testlogs")?;
    if !matches!(
        path.extension().and_then(|value| value.to_str()),
        Some("log" | "xml")
    ) {
        return None;
    }
    Some(components[..execroot].iter().collect())
}

fn set_test_log_unavailable(summary: &mut bazel_mcp_types::InvocationSummary, reason: &str) {
    for test in summary
        .tests
        .iter_mut()
        .filter(|test| test.status != TestStatus::Passed)
    {
        test.test_log_available = false;
        test.test_log_unavailable_reason = Some(reason.to_owned());
    }
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

fn invocation_ledger_row(record: &InvocationRecord) -> serde_json::Value {
    let arguments = record
        .request
        .arguments
        .iter()
        .take(3)
        .map(|argument| bounded_text(argument, 128))
        .collect::<Vec<_>>();
    let exit_code = match record.termination {
        Some(Termination::Exit { code }) => Some(code),
        _ => None,
    };
    serde_json::json!({
        "invocation_id": record.request.id,
        "workspace": bounded_text(&record.request.workspace.to_string_lossy(), 256),
        "state": record.state,
        "command": record.request.command,
        "arguments": arguments,
        "arguments_truncated": record.request.arguments.len() > 3,
        "requested_at_ms": record.request.requested_at_ms,
        "finished_at_ms": record.finished_at_ms,
        "exit_code": exit_code,
        "duration_ms": record.metrics.bazel_wall_ms,
        "headline": record
            .summary
            .as_ref()
            .map(|summary| bounded_text(&summary.headline, 256)),
        "targets": record.summary.as_ref().map(|summary| &summary.target_counts),
        "tests": record.summary.as_ref().map(|summary| &summary.test_counts),
        "raw_output_bytes": record.metrics.raw_output_bytes,
        "model_visible_bytes": record.metrics.model_visible_bytes,
        "inspect_calls": record.metrics.inspect_calls,
    })
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

    #[test]
    fn log_evidence_is_automatic_deduplicated_and_encoding_neutral() {
        let records = failure_evidence_records(
            &BazelCommand::Test,
            true,
            b"TEST_ROOT_CAUSE\nordinary stdout\n",
            b"ERROR: build wrapper\nTEST_ROOT_CAUSE\nordinary stderr\n",
        );
        assert_eq!(records[0].text, "TEST_ROOT_CAUSE [repeated 2 times]");
        assert!(
            records
                .iter()
                .any(|record| record.text == "ordinary stdout")
        );
        assert!(
            records
                .iter()
                .any(|record| record.text == "ordinary stderr")
        );
        let public = records
            .iter()
            .map(|record| record.text.clone())
            .collect::<Vec<_>>();
        let value = serde_json::to_value(public).unwrap();
        assert!(
            value
                .as_array()
                .unwrap()
                .iter()
                .all(|item| item.is_string())
        );
        assert!(!value.to_string().contains("\"stdout\":"));
        assert!(!value.to_string().contains("\"stderr\":"));
    }

    #[test]
    fn evidence_cursor_advances_after_last_emitted_item_without_gaps() {
        let id = InvocationId::new();
        let records = (0..7)
            .map(|index| EvidenceRecord {
                label: None,
                text: format!("ERROR: item {index}"),
            })
            .collect::<Vec<_>>();
        let mut request = InspectRequest {
            invocation_id: Some(id),
            workspace: None,
            state: None,
            command: None,
            view: InspectView::Log,
            cursor: None,
            filter: None,
            limit: 2,
            max_bytes: 8 * 1024,
        };
        let mut observed = Vec::new();
        loop {
            let start = request
                .cursor
                .as_deref()
                .map(|cursor| LogCursor::decode_for(cursor, id, InspectView::Log, None).unwrap())
                .map_or(0, |cursor| cursor.next_record);
            let (items, truncated, cursor) =
                page_evidence_records(records.clone().into_iter(), start, &request, id).unwrap();
            observed.extend(items);
            request.cursor = cursor;
            if !truncated {
                break;
            }
        }
        assert_eq!(
            observed,
            (0..7)
                .map(|index| format!("ERROR: item {index}"))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn associates_test_artifacts_without_exposing_a_public_uri() {
        let artifact = bazel_mcp_types::Artifact {
            name: "test.log".into(),
            kind: ArtifactKind::TestLog,
            uri: "file:///tmp/output/execroot/ws/bazel-out/testlogs/pkg/failing/test.log".into(),
            size_bytes: None,
            locally_available: true,
        };
        assert!(artifact_matches_test(&artifact, "//pkg:failing"));
        assert!(!artifact_matches_test(&artifact, "//pkg:passing"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn default_bazel_testlog_containment_accepts_real_paths_and_rejects_symlink_escapes() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let store = Store::open(root.path().join("store")).await.unwrap();
        let service = InvocationService::new(store, RunnerConfig::default()).unwrap();
        let workspace = root.path().join("workspace");
        let testlogs = root
            .path()
            .join("output-base/execroot/ws/bazel-out/config/testlogs/pkg/test");
        tokio::fs::create_dir_all(&workspace).await.unwrap();
        tokio::fs::create_dir_all(&testlogs).await.unwrap();
        let safe = testlogs.join("test.log");
        tokio::fs::write(&safe, "failure").await.unwrap();
        let artifact = bazel_mcp_types::Artifact {
            name: "test.log".into(),
            kind: ArtifactKind::TestLog,
            uri: format!("file://{}", safe.display()),
            size_bytes: None,
            locally_available: true,
        };
        let canonical_safe = tokio::fs::canonicalize(&safe).await.unwrap();
        assert_eq!(
            service.validated_artifact_path(&workspace, &artifact).await,
            Some(canonical_safe)
        );

        let outside = root.path().join("outside.log");
        tokio::fs::write(&outside, "secret").await.unwrap();
        let escaped = testlogs.join("escaped.log");
        symlink(&outside, &escaped).unwrap();
        let escaped_artifact = bazel_mcp_types::Artifact {
            uri: format!("file://{}", escaped.display()),
            ..artifact
        };
        assert_eq!(
            service
                .validated_artifact_path(&workspace, &escaped_artifact)
                .await,
            None
        );
    }
}
