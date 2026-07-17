use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use bazel_mcp_bes::{BesError, BesServer};
use bazel_mcp_policy::{
    PolicyConfig, PolicyError, Redactor, effective_output_base, filtered_environment,
    resolve_bazel_executable, validate_arguments, validate_command, validate_query_arguments,
    validate_workspace,
};
use bazel_mcp_reducer::{ReducerPipeline, StarlarkReducerConfig, load_starlark_reducers};
use bazel_mcp_store::{InvocationCompletion, InvocationPaths, Store, StoreError};
use bazel_mcp_types::{
    DeferredFailure, DeferredFailureKind, DeferredResultRecord, DeferredResultView,
    DeferredRetrieval, DeferredTerminalState, InvocationId, InvocationRecord, InvocationRequest,
    InvocationState, Page, PageRequest, ResultDisposition, Termination,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{Mutex, OwnedSemaphorePermit};
use tokio_util::sync::CancellationToken;

use crate::{
    execution::cancelled_summary,
    output_base_lock::{
        OutputBaseLockAcquisition, OutputBaseWaitStatus, acquire as acquire_output_base_lock,
        default_output_base_lock_root,
    },
    scheduler::InvocationScheduler,
    version::detect_bazel_version,
};

#[cfg(test)]
use crate::inspection::{
    EvidenceRecord, InspectRequest, LogCursor, failure_evidence_records, page_evidence_records,
    should_persist_failure_evidence,
};

#[cfg(test)]
use crate::{artifacts::local_artifact_path, test_evidence::artifact_matches_test};

#[cfg(test)]
use bazel_mcp_types::{ArtifactKind, BazelCommand, InspectView};

pub(crate) const COMPLETE_BEP_LOG_LIMIT: usize = 2 * 1024 * 1024;
pub(crate) const FALLBACK_LOG_LIMIT: usize = 8 * 1024 * 1024;
pub(crate) const BES_COMPLETION_GRACE: Duration = Duration::from_secs(2);
const DURABLE_LIFECYCLE_RECHECK: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BepTransport {
    #[default]
    Tail,
    /// POSIX FIFO fast path. Falls back to regular-file tailing when FIFO
    /// setup or Bazel server PID discovery is unavailable.
    Fifo,
    Bes,
}

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
    pub output_base_lock_root: PathBuf,
    pub bep_transport: BepTransport,
    pub starlark_reducers: StarlarkReducerConfig,
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
            supported_bazel_major_versions: [8, 9].into_iter().collect(),
            allow_unsupported_bazel_versions: false,
            version_check_timeout: Duration::from_secs(30),
            maximum_pending_invocations: 256,
            output_base_lock_root: default_output_base_lock_root(),
            bep_transport: BepTransport::Tail,
            starlark_reducers: StarlarkReducerConfig::default(),
        }
    }
}

impl RunnerConfig {
    /// Validate the settings required to construct an invocation service.
    ///
    /// Keeping this check on the runner configuration lets configuration
    /// frontends reject invalid settings without duplicating the runner's
    /// invariants.
    pub fn validate(&self) -> Result<(), RunnerError> {
        if self.global_concurrency == 0 {
            return Err(RunnerError::InvalidConfiguration(
                "global concurrency must be greater than zero",
            ));
        }
        if self.maximum_pending_invocations < self.global_concurrency {
            return Err(RunnerError::InvalidConfiguration(
                "maximum pending invocations must be at least global concurrency",
            ));
        }
        if self.maximum_timeout.is_zero() {
            return Err(RunnerError::InvalidConfiguration(
                "maximum timeout must be greater than zero",
            ));
        }
        if self.default_timeout > self.maximum_timeout {
            return Err(RunnerError::InvalidConfiguration(
                "default timeout exceeds maximum timeout",
            ));
        }
        if self.version_check_timeout.is_zero() {
            return Err(RunnerError::InvalidConfiguration(
                "version check timeout must be greater than zero",
            ));
        }
        if self.isolated_bazel_server_idle_timeout.is_zero() {
            return Err(RunnerError::InvalidConfiguration(
                "isolated Bazel server idle timeout must be greater than zero",
            ));
        }
        if self.output_base_lock_root.as_os_str().is_empty() {
            return Err(RunnerError::InvalidConfiguration(
                "output-base lock root must not be empty",
            ));
        }
        if !self.allow_unsupported_bazel_versions && self.supported_bazel_major_versions.is_empty()
        {
            return Err(RunnerError::InvalidConfiguration(
                "supported Bazel major versions must not be empty",
            ));
        }
        Ok(())
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
    #[error(transparent)]
    Bes(#[from] BesError),
    #[error(
        "BES transport cannot be combined with a caller-supplied --bes_backend; select tail transport to preserve the remote BES"
    )]
    BesBackendConflict,
    #[error("BES transport owns --bes_upload_mode so capture is complete before reduction")]
    BesUploadModeConflict,
    #[error("invalid custom reducer configuration: {0}")]
    ReducerConfiguration(String),
}

#[derive(Clone)]
pub struct InvocationService {
    pub(crate) store: Store,
    pub(crate) config: RunnerConfig,
    pub(crate) redactor: Redactor,
    scheduler: InvocationScheduler,
    version_cache: Arc<Mutex<HashMap<VersionCacheKey, u32>>>,
    pub(crate) bes: Option<BesServer>,
    pub(crate) reducers: ReducerPipeline,
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
    explicit_output_base: Option<PathBuf>,
    output_base_wait: Arc<OutputBaseWaitStatus>,
    executable: PathBuf,
    cancellation: CancellationToken,
    _pending_permit: OwnedSemaphorePermit,
    deferred: bool,
}

/// Narrow store access used by the runner's process-level integration tests.
/// Production callers should use the invocation and inspection APIs instead.
#[doc(hidden)]
#[derive(Clone, Copy)]
pub struct RunnerTestSupport<'a> {
    store: &'a Store,
}

#[doc(hidden)]
impl RunnerTestSupport<'_> {
    pub async fn get_invocation(self, id: InvocationId) -> Result<InvocationRecord, StoreError> {
        self.store.get_invocation(id).await
    }

    #[must_use]
    pub fn paths_for(self, record: &InvocationRecord) -> InvocationPaths {
        self.store.paths_for(record)
    }

    pub async fn replace_artifacts(
        self,
        id: InvocationId,
        artifacts: &[bazel_mcp_types::Artifact],
    ) -> Result<(), StoreError> {
        self.store.replace_artifacts(id, artifacts).await
    }

    pub async fn create_invocation(
        self,
        record: &InvocationRecord,
    ) -> Result<InvocationPaths, StoreError> {
        self.store.create_invocation(record).await
    }

    pub async fn transition(
        self,
        id: InvocationId,
        next: InvocationState,
        termination: Option<Termination>,
        summary: Option<bazel_mcp_types::InvocationSummary>,
    ) -> Result<InvocationRecord, StoreError> {
        self.store.transition(id, next, termination, summary).await
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvocationProgress {
    pub state: InvocationState,
    pub phase: Option<&'static str>,
    pub output_base_lock_wait_ms: u64,
    pub output_base_lock_owner: Option<String>,
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
        if config.bep_transport == BepTransport::Bes {
            return Err(RunnerError::InvalidConfiguration(
                "BES transport requires asynchronous InvocationService::start",
            ));
        }
        Self::new_with_bes(store, config, None)
    }

    pub async fn start(store: Store, config: RunnerConfig) -> Result<Self, RunnerError> {
        let bes = if config.bep_transport == BepTransport::Bes {
            Some(BesServer::start().await?)
        } else {
            None
        };
        Self::new_with_bes(store, config, bes)
    }

    fn new_with_bes(
        store: Store,
        config: RunnerConfig,
        bes: Option<BesServer>,
    ) -> Result<Self, RunnerError> {
        config.validate()?;
        let redactor = Redactor::new(&config.policy.redaction_patterns)?;
        let reducers = load_starlark_reducers(&config.starlark_reducers).map_err(|error| {
            RunnerError::ReducerConfiguration(redactor.redact_bounded(&error.to_string(), 8 * 1024))
        })?;
        Ok(Self {
            store,
            scheduler: InvocationScheduler::new(
                config.global_concurrency,
                config.maximum_pending_invocations,
            ),
            config,
            redactor,
            version_cache: Arc::new(Mutex::new(HashMap::new())),
            bes,
            reducers,
        })
    }

    #[doc(hidden)]
    #[must_use]
    pub fn test_support(&self) -> RunnerTestSupport<'_> {
        RunnerTestSupport { store: &self.store }
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
        self.scheduler.remove(id);
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
            supervisor.scheduler.remove(id);
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
            if let Some(mut lifecycle) = self.scheduler.lifecycle(id) {
                loop {
                    if lifecycle.borrow_and_update().is_terminal() {
                        return Ok(self.store.get_invocation(id).await?);
                    }
                    tokio::select! {
                        () = cancellation.cancelled() => {
                            return Err(RunnerError::WaitCancelled(id));
                        }
                        changed = lifecycle.changed() => {
                            if changed.is_err() {
                                break;
                            }
                        }
                    }
                }
            }

            if self
                .store
                .get_invocation_header(id)
                .await?
                .state
                .is_terminal()
            {
                return Ok(self.store.get_invocation(id).await?);
            }

            // A restarted service has no in-memory publisher, while another
            // process may still own the durable invocation. Recheck that rare
            // fallback at the store generation cadence; live work never polls.
            tokio::select! {
                () = cancellation.cancelled() => return Err(RunnerError::WaitCancelled(id)),
                () = tokio::time::sleep(DURABLE_LIFECYCLE_RECHECK) => {}
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
        if self.config.bep_transport == BepTransport::Bes
            && request
                .arguments
                .iter()
                .any(|argument| is_flag(argument, "--bes_backend"))
        {
            return Err(RunnerError::BesBackendConflict);
        }
        if self.config.bep_transport == BepTransport::Bes
            && request
                .arguments
                .iter()
                .any(|argument| is_flag(argument, "--bes_upload_mode"))
        {
            return Err(RunnerError::BesUploadModeConflict);
        }
        let pending_permit = self
            .scheduler
            .try_acquire_pending()
            .ok_or(RunnerError::QueueFull(
                self.config.maximum_pending_invocations,
            ))?;
        let workspace = validate_workspace(&request.workspace, &self.config.policy.allowed_roots)?;
        let explicit_output_base = effective_output_base(&workspace, &request.startup_arguments)?;
        let lock_key = explicit_output_base
            .clone()
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
        let output_base_wait = Arc::new(OutputBaseWaitStatus::default());
        self.scheduler.register(
            id,
            cancellation.clone(),
            output_base_wait.clone(),
            stored.state,
        );

        Ok(PreparedSubmission {
            queued,
            paths,
            lock_key,
            explicit_output_base,
            output_base_wait,
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
            explicit_output_base,
            output_base_wait,
            executable,
            cancellation,
            _pending_permit,
            deferred: _,
        } = prepared;
        let id = queued.request.id;
        async {
            let workspace_lock = self.scheduler.workspace_lock(&lock_key).await;
            let workspace_guard = tokio::select! {
                guard = workspace_lock.lock() => guard,
                () = cancellation.cancelled() => {
                    return self.finish_cancelled(id).await;
                }
            };
            let output_base_guard = match acquire_output_base_lock(
                &self.config.output_base_lock_root,
                &lock_key,
                id,
                cancellation.clone(),
                output_base_wait.clone(),
            )
            .await
            {
                Ok(OutputBaseLockAcquisition::Acquired(guard)) => guard,
                Ok(OutputBaseLockAcquisition::Cancelled) => {
                    return self.finish_cancelled(id).await;
                }
                Err(error) => {
                    return self
                        .finish_start_failure(
                            id,
                            &format!("could not acquire output-base coordination lock: {error}"),
                        )
                        .await;
                }
            };
            let permit = tokio::select! {
                permit = self.scheduler.acquire_execution() => {
                    permit.ok_or(RunnerError::SchedulerClosed)?
                }
                () = cancellation.cancelled() => {
                    return self.finish_cancelled(id).await;
                }
            };

            if cancellation.is_cancelled() {
                return self.finish_cancelled(id).await;
            }
            if let Err(error) = self
                .transition_invocation(id, InvocationState::Starting, None, None)
                .await
            {
                let message = self.redactor.redact_bounded(
                    &format!("could not record Bazel starting state: {error}"),
                    1_000,
                );
                tracing::warn!(invocation_id = %id, %message);
                if self
                    .store
                    .get_invocation_header(id)
                    .await
                    .is_ok_and(|record| !record.state.is_terminal())
                {
                    let _ = self
                        .transition_invocation(
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
                if let Ok(record) = self.store.get_invocation_header(id).await
                    && record.state.is_terminal()
                    && record.summary.is_some()
                {
                    return Ok(record.into_record());
                }
                return Err(error.into());
            }
            let result = self
                .execute(
                    &queued,
                    &paths,
                    &executable,
                    cancellation.clone(),
                    explicit_output_base.as_deref(),
                    output_base_wait,
                )
                .await;
            drop(workspace_guard);
            drop(output_base_guard);
            drop(permit);
            result
        }
        .await
    }

    async fn finish_start_failure(
        &self,
        id: InvocationId,
        message: &str,
    ) -> Result<InvocationRecord, RunnerError> {
        let message = self.redactor.redact_bounded(message, 1_000);
        let current = self.store.get_invocation(id).await?;
        if current.state.is_terminal() {
            return Ok(current);
        }
        if current.state == InvocationState::Queued
            && let Err(error) = self
                .transition_invocation(id, InvocationState::Starting, None, None)
                .await
            && !matches!(error, StoreError::State(_))
        {
            return Err(error.into());
        }
        let current = self.store.get_invocation(id).await?;
        if current.state.is_terminal() {
            return Ok(current);
        }
        self.transition_invocation(
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
        .await
        .map_err(Into::into)
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
            .get_invocation_header(id)
            .await
            .is_ok_and(|record| !record.state.is_terminal())
        {
            let _ = self
                .transition_invocation(
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
        self.scheduler.cancel_all()
    }

    pub async fn active_invocation_count(&self) -> usize {
        self.scheduler.active_count()
    }

    /// Wait until every invocation owned by this process has left the live
    /// registry. Lifecycle changes wake this future without periodic checks.
    pub async fn wait_until_idle(&self) {
        self.scheduler.wait_until_idle().await;
    }

    pub async fn cancel_with_reason(
        &self,
        id: InvocationId,
        reason: Option<&str>,
    ) -> Result<CancelResult, RunnerError> {
        let record = self.store.get_invocation_header(id).await?;
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
        let cancellation = self.scheduler.cancellation(id);
        if let Some(cancellation) = &cancellation {
            cancellation.cancel();
        }
        if record.state == InvocationState::Queued {
            match self
                .transition_invocation(
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
        let current_state = self.store.get_invocation_header(id).await?.state;
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
        if let Some(state) = self.scheduler.lifecycle_state(id) {
            return Ok(state);
        }
        Ok(self.store.get_invocation_header(id).await?.state)
    }

    pub async fn invocation_progress(
        &self,
        id: InvocationId,
    ) -> Result<InvocationProgress, RunnerError> {
        let state = self.invocation_state(id).await?;
        let wait = self.scheduler.output_base_wait(id);
        let wait = wait.map(|status| status.snapshot()).unwrap_or_else(|| {
            crate::output_base_lock::OutputBaseWaitSnapshot {
                active: false,
                elapsed_ms: 0,
                owner: None,
            }
        });
        Ok(InvocationProgress {
            state,
            phase: wait.active.then_some("output_base_lock_wait"),
            output_base_lock_wait_ms: wait.elapsed_ms,
            output_base_lock_owner: wait.owner,
        })
    }

    pub(crate) async fn transition_invocation(
        &self,
        id: InvocationId,
        next: InvocationState,
        termination: Option<Termination>,
        summary: Option<bazel_mcp_types::InvocationSummary>,
    ) -> Result<InvocationRecord, StoreError> {
        let record = self
            .store
            .transition(id, next, termination, summary)
            .await?;
        self.scheduler.publish_state(id, record.state);
        Ok(record)
    }

    pub(crate) async fn finish_invocation(
        &self,
        id: InvocationId,
        completion: InvocationCompletion,
    ) -> Result<InvocationRecord, StoreError> {
        let record = self.store.finish_invocation(id, completion).await?;
        self.scheduler.publish_state(id, record.state);
        Ok(record)
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
}

pub(crate) fn bounded_text(value: &str, maximum_bytes: usize) -> String {
    if value.len() <= maximum_bytes {
        return value.to_owned();
    }
    let mut boundary = maximum_bytes;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    format!("{}…", &value[..boundary])
}

fn is_flag(argument: &str, flag: &str) -> bool {
    argument == flag
        || argument
            .strip_prefix(flag)
            .is_some_and(|suffix| suffix.starts_with('='))
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn skips_eager_failure_evidence_only_for_successful_queries() {
        for command in [
            BazelCommand::Query,
            BazelCommand::Cquery,
            BazelCommand::Aquery,
        ] {
            assert!(!should_persist_failure_evidence(&command, false));
            assert!(should_persist_failure_evidence(&command, true));
        }
        assert!(should_persist_failure_evidence(&BazelCommand::Build, false));
        assert!(should_persist_failure_evidence(
            &BazelCommand::Version,
            false
        ));
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
            item_limit: 2,
            scan_limit: 100,
        };
        let mut observed = Vec::new();
        loop {
            let start = request
                .cursor
                .as_deref()
                .map(|cursor| LogCursor::decode_for(cursor, id, InspectView::Log, None).unwrap())
                .map_or(0, |cursor| cursor.next_record);
            let page = page_evidence_records(records.clone(), start, &request, id).unwrap();
            observed.extend(page.items);
            request.cursor = page.next_cursor;
            if !page.truncated {
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
    fn filtered_test_logs_include_bounded_same_target_context_across_pages() {
        let id = InvocationId::new();
        let label = Some("//pkg:process_test".to_owned());
        let records = vec![
            EvidenceRecord {
                label: label.clone(),
                text: "unrelated setup".to_owned(),
            },
            EvidenceRecord {
                label: label.clone(),
                text: "---- failing_case stdout ----".to_owned(),
            },
            EvidenceRecord {
                label: label.clone(),
                text: "thread 'failing_case' panicked at tests/process.rs:42:5".to_owned(),
            },
            EvidenceRecord {
                label,
                text: "called `Result::unwrap()` on an `Err` value: not found".to_owned(),
            },
            EvidenceRecord {
                label: Some("//pkg:other_test".to_owned()),
                text: "other target output".to_owned(),
            },
        ];
        let mut request = InspectRequest {
            invocation_id: Some(id),
            workspace: None,
            state: None,
            command: None,
            view: InspectView::TestLog,
            cursor: None,
            filter: Some("failing_case".to_owned()),
            item_limit: 2,
            scan_limit: 2,
        };
        let mut observed = Vec::new();
        loop {
            let start = request
                .cursor
                .as_deref()
                .map(|cursor| {
                    LogCursor::decode_for(
                        cursor,
                        id,
                        InspectView::TestLog,
                        request.filter.as_deref(),
                    )
                    .unwrap()
                })
                .map_or(0, |cursor| cursor.next_record);
            let page = page_evidence_records(records.clone(), start, &request, id).unwrap();
            observed.extend(page.items);
            request.cursor = page.next_cursor;
            if !page.truncated {
                break;
            }
        }
        assert_eq!(
            observed,
            vec![
                "unrelated setup",
                "---- failing_case stdout ----",
                "thread 'failing_case' panicked at tests/process.rs:42:5",
                "called `Result::unwrap()` on an `Err` value: not found",
            ]
        );
    }

    #[test]
    fn ordinary_log_filters_do_not_expand_to_adjacent_context() {
        let id = InvocationId::new();
        let records = vec![
            EvidenceRecord {
                label: None,
                text: "before".to_owned(),
            },
            EvidenceRecord {
                label: None,
                text: "ERROR: direct match".to_owned(),
            },
            EvidenceRecord {
                label: None,
                text: "after".to_owned(),
            },
        ];
        let request = InspectRequest {
            invocation_id: Some(id),
            workspace: None,
            state: None,
            command: None,
            view: InspectView::Log,
            cursor: None,
            filter: Some("direct match".to_owned()),
            item_limit: 20,
            scan_limit: 100,
        };
        let page = page_evidence_records(records, 0, &request, id).unwrap();
        assert_eq!(page.items, vec!["ERROR: direct match"]);
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
