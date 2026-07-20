//! Bazel process execution and terminal summary finalization.

use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    process::ExitStatus,
    sync::Arc,
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::io;

use bazel_mcp_bep::StreamOutcome;
use bazel_mcp_bes::CAPTURE_ID_HEADER;
use bazel_mcp_policy::filtered_environment;
use bazel_mcp_reducer::{
    BepAccumulator, Budget, REDUCER_API_VERSION, ReducerContext, RunBepOutcome,
    StreamReductionOutput, finalize_diagnostics, normalize_terminal_text,
};
use bazel_mcp_store::{InvocationCompletion, InvocationPaths, StoreError};
use bazel_mcp_types::{
    BazelCommand, CommandClass, Diagnostic, DiagnosticCategory, InspectHint, InvocationId,
    InvocationMetrics, InvocationRecord, InvocationRequest, InvocationState, InvocationSummary,
    PageRequest, QueryRow, RunOutcome, RunSummary, Severity, Termination, TestStatus,
};
use tokio::{
    process::{Child, Command},
    task,
};
use tokio_util::sync::CancellationToken;

use crate::{
    cancel::{ProcessGroupGuard, terminate_child},
    capture,
    driver::ExecutionDriver,
    inspection::should_persist_failure_evidence,
    output_base_lock::{NativeOutputBaseWaitObserver, OutputBaseWaitStatus},
    service::{
        BES_COMPLETION_GRACE, BepTransport, COMPLETE_BEP_LOG_LIMIT, FALLBACK_LOG_LIMIT,
        InvocationService, RunnerError, bounded_text,
    },
};

#[cfg(unix)]
use crate::service::RunnerConfig;

/// Immutable inputs selected before Bazel is spawned.
///
/// Owning these values prevents later phases from reaching back into queued
/// state or reconstructing transport and timeout decisions.
struct ExecutionPlan {
    request: InvocationRequest,
    paths: InvocationPaths,
    driver: ExecutionDriver,
    cancellation: CancellationToken,
    explicit_output_base: Option<PathBuf>,
    output_base_wait: Arc<OutputBaseWaitStatus>,
    extension_limits: Option<(usize, usize)>,
    queue_ms: u64,
    timeout: Duration,
}

/// A spawned process together with every guard required to shut it down and
/// finish its live evidence stream.
struct RunningInvocation {
    plan: ExecutionPlan,
    child: Child,
    process_group: ProcessGroupGuard,
    native_output_base_wait: NativeOutputBaseWaitObserver,
    live_bep: Option<capture::LiveBepCapture>,
    started: Instant,
}

/// Process termination is known, but durable evidence has not yet been
/// drained and reduced.
struct ExitedInvocation {
    plan: ExecutionPlan,
    status: Option<ExitStatus>,
    termination: Termination,
    state: InvocationState,
    live_bep: Option<capture::LiveBepCapture>,
    bazel_wall_ms: u64,
    output_base_lock_wait_ms: u64,
}

/// Raw local evidence and its decoded BEP accumulator. Redaction and public
/// byte budgets deliberately happen only in the next phase.
struct CapturedEvidence {
    exited: ExitedInvocation,
    bep: BepAccumulator,
    bep_outcome: StreamOutcome,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    query_row_count: u64,
    query_sample: Vec<QueryRow>,
    reduction_started: Instant,
}

/// A fully redacted and enriched result ready for its single durable commit.
struct ReducedOutcome {
    id: InvocationId,
    completion: InvocationCompletion,
}

/// The only phase allowed to move an invocation into a terminal state.
struct TerminalCommit {
    id: InvocationId,
    completion: InvocationCompletion,
    recover_without_evidence: bool,
}

enum StartExecution {
    Running(Box<RunningInvocation>),
    Terminal(Box<TerminalCommit>),
}

#[derive(Clone)]
struct TerminalContext {
    id: InvocationId,
    workspace: PathBuf,
    state: InvocationState,
    termination: Termination,
    elapsed_ms: u64,
}

impl ExecutionPlan {
    fn new(
        service: &InvocationService,
        queued: &InvocationRecord,
        paths: &InvocationPaths,
        driver: &ExecutionDriver,
        cancellation: CancellationToken,
        explicit_output_base: Option<&Path>,
        output_base_wait: Arc<OutputBaseWaitStatus>,
    ) -> Self {
        let queue_ms = u64::try_from(
            bazel_mcp_types::unix_timestamp_ms().saturating_sub(queued.request.requested_at_ms),
        )
        .unwrap_or_default();
        let timeout = queued
            .request
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(service.config.default_timeout)
            .max(Duration::from_secs(1))
            .min(service.config.maximum_timeout);
        let extension_limits = (!service.reducers.is_empty()).then_some((
            service.config.starlark_reducers.limits.max_events,
            service.config.starlark_reducers.limits.max_input_bytes,
        ));
        Self {
            request: queued.request.clone(),
            paths: paths.clone(),
            driver: driver.clone(),
            cancellation,
            explicit_output_base: explicit_output_base.map(Path::to_owned),
            output_base_wait,
            extension_limits,
            queue_ms,
            timeout,
        }
    }

    fn failure_context(&self) -> TerminalContext {
        TerminalContext {
            id: self.request.id,
            workspace: self.request.workspace.clone(),
            state: InvocationState::Failed,
            termination: Termination::Interrupted,
            elapsed_ms: 0,
        }
    }
}

impl RunningInvocation {
    fn failure_context(&self) -> TerminalContext {
        TerminalContext {
            id: self.plan.request.id,
            workspace: self.plan.request.workspace.clone(),
            state: InvocationState::Failed,
            termination: Termination::Interrupted,
            elapsed_ms: duration_millis(self.started.elapsed()),
        }
    }
}

impl ExitedInvocation {
    fn terminal_context(&self) -> TerminalContext {
        TerminalContext {
            id: self.plan.request.id,
            workspace: self.plan.request.workspace.clone(),
            state: self.state,
            termination: self.termination.clone(),
            elapsed_ms: self.bazel_wall_ms,
        }
    }
}

impl ReducedOutcome {
    fn into_terminal(self) -> TerminalCommit {
        TerminalCommit {
            id: self.id,
            completion: self.completion,
            recover_without_evidence: false,
        }
    }
}

impl TerminalCommit {
    fn failure(
        id: InvocationId,
        termination: Termination,
        summary: InvocationSummary,
        queue_ms: u64,
    ) -> Self {
        Self {
            id,
            completion: InvocationCompletion {
                state: InvocationState::Failed,
                termination,
                summary,
                run: None,
                metrics: InvocationMetrics {
                    queue_ms,
                    ..Default::default()
                },
                canonical_arguments: None,
                artifacts: Vec::new(),
            },
            recover_without_evidence: false,
        }
    }

    fn cancelled(id: InvocationId, queue_ms: u64) -> Self {
        Self {
            id,
            completion: InvocationCompletion {
                state: InvocationState::Cancelled,
                termination: Termination::Cancelled,
                summary: cancelled_summary(),
                run: None,
                metrics: InvocationMetrics {
                    queue_ms,
                    ..Default::default()
                },
                canonical_arguments: None,
                artifacts: Vec::new(),
            },
            recover_without_evidence: false,
        }
    }
}

impl InvocationService {
    pub(crate) async fn execute(
        &self,
        queued: &InvocationRecord,
        paths: &InvocationPaths,
        driver: &ExecutionDriver,
        cancellation: CancellationToken,
        explicit_output_base: Option<&Path>,
        output_base_wait: Arc<OutputBaseWaitStatus>,
    ) -> Result<InvocationRecord, RunnerError> {
        let plan = ExecutionPlan::new(
            self,
            queued,
            paths,
            driver,
            cancellation,
            explicit_output_base,
            output_base_wait,
        );
        let starting_context = plan.failure_context();
        let started = match self.start_execution(plan).await {
            Ok(started) => started,
            Err(error) => return self.recover_terminal_failure(starting_context, error).await,
        };
        let running = match started {
            StartExecution::Running(running) => *running,
            StartExecution::Terminal(commit) => {
                return match self.commit_terminal(*commit).await {
                    Ok(record) => Ok(record),
                    Err(error) => self.recover_terminal_failure(starting_context, error).await,
                };
            }
        };
        let running_context = running.failure_context();
        let exited = match self.wait_for_exit(running).await {
            Ok(exited) => exited,
            Err(error) => return self.recover_terminal_failure(running_context, error).await,
        };
        let terminal_context = exited.terminal_context();
        let result = async {
            let captured = self.capture_evidence(exited).await?;
            let reduced = self.reduce_evidence(captured).await?;
            self.commit_terminal(reduced.into_terminal()).await
        }
        .await;
        match result {
            Ok(record) => Ok(record),
            Err(error) => self.recover_terminal_failure(terminal_context, error).await,
        }
    }

    async fn start_execution(&self, plan: ExecutionPlan) -> Result<StartExecution, RunnerError> {
        let queued = &plan;
        let paths = &plan.paths;
        let driver = plan.driver.clone();
        let command_class = driver.command_class(&queued.request.command);
        let bep_transport = if driver.is_aspect() {
            BepTransport::Bes
        } else {
            self.config.bep_transport
        };
        let cancellation = plan.cancellation.clone();
        let explicit_output_base = plan.explicit_output_base.as_deref();
        let output_base_wait = plan.output_base_wait.clone();
        let queue_ms = plan.queue_ms;
        if cancellation.is_cancelled() {
            return Ok(StartExecution::Terminal(Box::new(
                TerminalCommit::cancelled(queued.request.id, queue_ms),
            )));
        }
        let (stdout, stderr) = match capture::open_stdio(paths).await {
            Ok(streams) => streams,
            Err(error) => {
                let message = self.redactor.redact_bounded(
                    &format!("could not create Bazel evidence files: {error}"),
                    1_000,
                );
                let summary = InvocationSummary {
                    success: false,
                    headline: format!("Could not prepare Bazel invocation: {message}"),
                    truncated: true,
                    ..Default::default()
                };
                return Ok(StartExecution::Terminal(Box::new(TerminalCommit::failure(
                    queued.request.id,
                    Termination::SpawnFailure { message },
                    summary,
                    queue_ms,
                ))));
            }
        };
        let native_output_base_wait = NativeOutputBaseWaitObserver::start(
            paths.stdout.clone(),
            paths.stderr.clone(),
            paths.bep.clone(),
            explicit_output_base.map(Path::to_owned),
            output_base_wait.clone(),
        )
        .await;
        #[cfg(unix)]
        let mut prepared_fifo = if captures_bep(command_class)
            && !driver.is_aspect()
            && bep_transport == BepTransport::Fifo
        {
            match capture::PreparedFifoBepCapture::prepare(&paths.bep) {
                Ok(prepared) => match probe_bazel_server_pid(
                    driver.bazel_executable(),
                    &queued.request.workspace,
                    &queued.request.startup_arguments,
                    &self.config,
                    cancellation.clone(),
                )
                .await
                {
                    Ok(server_pid) => Some((prepared, server_pid)),
                    Err(error) => {
                        tracing::warn!(
                            invocation_id = %queued.request.id,
                            %error,
                            "could not discover Bazel server PID; falling back to BEP file tail"
                        );
                        None
                    }
                },
                Err(error) => {
                    tracing::warn!(
                        invocation_id = %queued.request.id,
                        %error,
                        "could not prepare BEP FIFO; falling back to BEP file tail"
                    );
                    None
                }
            }
        } else {
            None
        };
        #[cfg(not(unix))]
        if captures_bep(command_class) && !driver.is_aspect() && bep_transport == BepTransport::Fifo
        {
            tracing::debug!(
                invocation_id = %queued.request.id,
                "BEP FIFO transport is unavailable on this platform; using file tail"
            );
        }
        if cancellation.is_cancelled() {
            return Ok(StartExecution::Terminal(Box::new(
                TerminalCommit::cancelled(queued.request.id, queue_ms),
            )));
        }
        let extension_limits = plan.extension_limits;
        let bes_bep = if captures_bep(command_class) && bep_transport == BepTransport::Bes {
            match &self.bes {
                Some(server) => Some(capture::LiveBepCapture::Bes(capture::BesBepCapture::start(
                    server.register(queued.request.id.to_string())?,
                    paths.bep.clone(),
                    extension_limits,
                )?)),
                None => None,
            }
        } else {
            None
        };
        let mut command = Command::new(driver.executable());
        command
            .current_dir(&queued.request.workspace)
            .env_clear()
            .envs(filtered_environment(&self.config.policy));
        match &driver {
            ExecutionDriver::Bazel { .. } => {
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
                if captures_bep(command_class) {
                    command.arg(format!("--invocation_id={}", queued.request.id));
                    match bep_transport {
                        BepTransport::Tail => {
                            command
                                .arg(format!("--build_event_binary_file={}", paths.bep.display()))
                                .arg("--build_event_binary_file_path_conversion=false");
                        }
                        BepTransport::Fifo => {
                            #[cfg(unix)]
                            let output = prepared_fifo
                                .as_ref()
                                .map_or(paths.bep.as_path(), |(prepared, _)| prepared.path());
                            #[cfg(not(unix))]
                            let output = paths.bep.as_path();
                            command
                                .arg(format!("--build_event_binary_file={}", output.display()))
                                .arg("--build_event_binary_file_path_conversion=false");
                        }
                        BepTransport::Bes => {
                            let endpoint = self
                                .bes
                                .as_ref()
                                .ok_or(RunnerError::InvalidConfiguration(
                                    "BES transport was not initialized",
                                ))?
                                .endpoint();
                            command
                                .arg(format!("--bes_backend={endpoint}"))
                                .arg("--bes_upload_mode=wait_for_upload_complete");
                        }
                    }
                    command.args([
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
                    if queued.request.command == BazelCommand::Run {
                        // Command-line values override bazelrc defaults. Keep execution
                        // enabled while preventing terminal and BEP residue disclosure.
                        command.args([
                            "--run=true",
                            "--omit_run_args=true",
                            "--noexperimental_run_bep_event_include_residue",
                            "--subcommands=false",
                        ]);
                    }
                }
                command.args(&queued.request.arguments);
                if queued.request.command == BazelCommand::Run {
                    command
                        .arg(
                            queued
                                .request
                                .target
                                .as_deref()
                                .expect("validated run target"),
                        )
                        .arg("--")
                        .args(&queued.request.program_arguments);
                }
            }
            ExecutionDriver::Aspect {
                bazel_executable, ..
            } => {
                command
                    .env("BAZEL_REAL", bazel_executable)
                    .arg(format!("--task:id={}", queued.request.id))
                    .arg("--task:timing-summary=none")
                    .arg(queued.request.command.as_str());
                if let Some(output_user_root) = &self.config.output_user_root {
                    command
                        .arg(format!(
                            "--bazel-startup-flag=--output_user_root={}",
                            output_user_root.display()
                        ))
                        .arg(format!(
                            "--bazel-startup-flag=--max_idle_secs={}",
                            self.config.isolated_bazel_server_idle_timeout.as_secs()
                        ));
                }
                for argument in &queued.request.startup_arguments {
                    command.arg(format!("--bazel-startup-flag={argument}"));
                }
                if command_class == CommandClass::BuildLike {
                    for argument in [
                        format!("--invocation_id={}", queued.request.id),
                        "--tool_tag=bazel-mcp".to_owned(),
                        "--color=no".to_owned(),
                        "--curses=no".to_owned(),
                        "--show_progress=false".to_owned(),
                        "--show_result=0".to_owned(),
                    ] {
                        command.arg(format!("--bazel-flag={argument}"));
                    }
                    if matches!(
                        queued.request.command,
                        BazelCommand::Test | BazelCommand::Coverage
                    ) {
                        command
                            .arg("--bazel-flag=--test_output=errors")
                            .arg("--bazel-flag=--test_summary=none");
                    }
                    let endpoint = self
                        .bes
                        .as_ref()
                        .ok_or(RunnerError::InvalidConfiguration(
                            "Aspect capture BES was not initialized",
                        ))?
                        .endpoint();
                    command
                        .arg(format!("--bes-backend={endpoint}"))
                        .arg(format!(
                            "--bes-header={CAPTURE_ID_HEADER}={}",
                            queued.request.id
                        ));
                }
                command.args(&queued.request.arguments);
            }
        }
        command
            .stdin(std::process::Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .kill_on_drop(true);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.as_std_mut().process_group(0);
        }

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let message = self.redactor.redact_bounded(&error.to_string(), 1_000);
                let summary = InvocationSummary {
                    success: false,
                    headline: format!("Could not start {}: {message}", driver.display_name()),
                    ..Default::default()
                };
                return Ok(StartExecution::Terminal(Box::new(TerminalCommit::failure(
                    queued.request.id,
                    Termination::SpawnFailure { message },
                    summary,
                    queue_ms,
                ))));
            }
        };
        #[cfg(unix)]
        let fifo_bep = prepared_fifo.take().map(|(prepared, server_pid)| {
            let client_pid = child.id().unwrap_or_default();
            capture::LiveBepCapture::Fifo(prepared.start(
                paths.bep.clone(),
                server_pid,
                client_pid,
                extension_limits,
            ))
        });
        let process_group = ProcessGroupGuard::for_child(&child);
        if let Err(error) = self
            .transition_invocation(queued.request.id, InvocationState::Running, None, None)
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
            return Ok(StartExecution::Terminal(Box::new(TerminalCommit::failure(
                queued.request.id,
                Termination::Interrupted,
                InvocationSummary {
                    success: false,
                    headline: format!("Could not record Bazel execution state: {message}"),
                    truncated: true,
                    inspect_hint: Some(InspectHint::Log),
                    ..Default::default()
                },
                queue_ms,
            ))));
        }
        #[cfg(unix)]
        let incremental_bep = fifo_bep.or(bes_bep).or_else(|| {
            (captures_bep(command_class) && bep_transport != BepTransport::Bes).then(|| {
                capture::LiveBepCapture::Tail(capture::IncrementalBepCapture::start(
                    paths.bep.clone(),
                    extension_limits,
                ))
            })
        });
        #[cfg(not(unix))]
        let incremental_bep = bes_bep.or_else(|| {
            (captures_bep(command_class) && bep_transport != BepTransport::Bes).then(|| {
                capture::LiveBepCapture::Tail(capture::IncrementalBepCapture::start(
                    paths.bep.clone(),
                    extension_limits,
                ))
            })
        });
        Ok(StartExecution::Running(Box::new(RunningInvocation {
            plan,
            child,
            process_group,
            native_output_base_wait,
            live_bep: incremental_bep,
            started: Instant::now(),
        })))
    }

    async fn wait_for_exit(
        &self,
        running: RunningInvocation,
    ) -> Result<ExitedInvocation, RunnerError> {
        let RunningInvocation {
            plan,
            mut child,
            mut process_group,
            native_output_base_wait,
            live_bep,
            started,
        } = running;
        let cancellation = plan.cancellation.clone();
        let timeout = plan.timeout;
        let timeout_sleep = tokio::time::sleep(timeout);
        tokio::pin!(timeout_sleep);
        let output_limit_stdout = plan.paths.stdout.clone();
        let output_limit_stderr = plan.paths.stderr.clone();
        let output_limit = wait_for_run_output_limit(
            &output_limit_stdout,
            &output_limit_stderr,
            self.config.maximum_run_output_bytes,
        );
        tokio::pin!(output_limit);
        let (mut status, mut termination, mut state) = tokio::select! {
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
            () = &mut output_limit, if plan.request.command == BazelCommand::Run => {
                let status = terminate_child(
                    &mut child,
                    self.config.cancellation_interrupt_grace,
                    self.config.cancellation_terminate_grace,
                ).await?;
                (
                    Some(status),
                    Termination::OutputLimit {
                        maximum_bytes: self.config.maximum_run_output_bytes,
                    },
                    InvocationState::Failed,
                )
            }
        };
        if plan.request.command == BazelCommand::Run
            && !matches!(
                termination,
                Termination::Cancelled | Termination::Timeout | Termination::OutputLimit { .. }
            )
            && capture::file_size(&plan.paths.stdout)
                .await
                .saturating_add(capture::file_size(&plan.paths.stderr).await)
                > self.config.maximum_run_output_bytes
        {
            termination = Termination::OutputLimit {
                maximum_bytes: self.config.maximum_run_output_bytes,
            };
            state = InvocationState::Failed;
            status = None;
        }
        process_group.disarm();
        native_output_base_wait.finish().await;
        let bazel_wall_ms = duration_millis(started.elapsed());
        Ok(ExitedInvocation {
            output_base_lock_wait_ms: plan.output_base_wait.snapshot().elapsed_ms,
            plan,
            status,
            termination,
            state,
            live_bep,
            bazel_wall_ms,
        })
    }

    async fn capture_evidence(
        &self,
        mut exited: ExitedInvocation,
    ) -> Result<CapturedEvidence, RunnerError> {
        let queued = &exited.plan;
        let paths = &exited.plan.paths;
        let command_class = queued.driver.command_class(&queued.request.command);
        let status = &exited.status;
        let reduction_started = Instant::now();
        let incremental_reduction = match exited.live_bep.take() {
            Some(capture) => {
                let reduction = capture.finish(BES_COMPLETION_GRACE).await?;
                tracing::debug!(
                    invocation_id = %exited.plan.request.id,
                    source = ?reduction.source,
                    finalize_ms = reduction.finalize_ms,
                    events = reduction.outcome.event_count,
                    bytes = reduction.outcome.decoded_bytes,
                    "completed incremental BEP reduction"
                );
                Some(reduction)
            }
            None => None,
        };
        let (query_row_count, query_sample) = if command_class == CommandClass::Query {
            let query_row_count = self.store.count_query_rows(queued.request.id).await?;
            let redactor = self.redactor.clone();
            let page = self
                .store
                .page_query_rows_mapped_into(
                    queued.request.id,
                    None,
                    PageRequest {
                        scan_limit: 3,
                        ..PageRequest::new(None, 3)
                    },
                    move |value, output| {
                        redactor.redact_bounded_into(value, 4 * 1024, output);
                    },
                )
                .await?;
            (query_row_count, page.items)
        } else {
            (0, Vec::new())
        };
        let (bep, bep_outcome) = match incremental_reduction {
            Some(reduction) => (reduction.accumulator, reduction.outcome),
            None => capture::reduce_bep(paths.bep.clone(), exited.plan.extension_limits).await?,
        };
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
        let failed = exit_code != Some(0);
        if should_persist_failure_evidence(&queued.request.command, failed) {
            self.persist_failure_evidence(
                paths,
                &queued.request.workspace,
                &queued.request.command,
                failed,
                &stdout,
                &stderr,
            )
            .await?;
        }
        Ok(CapturedEvidence {
            exited,
            bep,
            bep_outcome,
            stdout,
            stderr,
            query_row_count,
            query_sample,
            reduction_started,
        })
    }

    async fn reduce_evidence(
        &self,
        captured: CapturedEvidence,
    ) -> Result<ReducedOutcome, RunnerError> {
        let CapturedEvidence {
            exited,
            bep,
            bep_outcome,
            stdout,
            stderr,
            query_row_count,
            query_sample,
            reduction_started,
        } = captured;
        let queued = &exited.plan;
        let paths = &exited.plan.paths;
        let command_class = queued.driver.command_class(&queued.request.command);
        let exit_code = exited.status.as_ref().and_then(ExitStatus::code);
        let bazel_wall_ms = exited.bazel_wall_ms;
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
            reducer_events,
            reducer_input_truncated,
            run_bep_outcome,
        } = reduced.unwrap_or_else(|_| {
            tracing::warn!(
                invocation_id = %queued.request.id,
                "streaming BEP reducer panicked; using bounded fallback summary"
            );
            StreamReductionOutput {
                summary: fallback_summary(exit_code, bazel_wall_ms, &stderr, &stdout),
                artifacts: Vec::new(),
                canonical_arguments: None,
                reducer_events: Vec::new(),
                reducer_input_truncated: false,
                run_bep_outcome: RunBepOutcome::default(),
            }
        });
        let canonical_arguments = if queued.request.command == BazelCommand::Run {
            let mut arguments = queued.request.startup_arguments.clone();
            arguments.push("run".to_owned());
            arguments.extend(queued.request.arguments.iter().cloned());
            arguments.extend(queued.request.target.iter().cloned());
            if !queued.request.program_arguments.is_empty() {
                arguments.push(format!(
                    "[REDACTED_PROGRAM_ARGS:{}]",
                    queued.request.program_arguments.len()
                ));
            }
            Some(arguments)
        } else {
            canonical_arguments
        }
        .map(|mut arguments| {
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
        if command_class == CommandClass::Query && exit_code == Some(0) {
            summary.headline = format!("Bazel query returned {query_row_count} rows");
            summary.inspect_hint = (query_row_count > 0).then_some(InspectHint::QueryResults);
            summary.query_result_count = Some(query_row_count);
            summary.query_sample = query_sample;
        } else if matches!(
            command_class,
            CommandClass::Informational | CommandClass::Unknown
        ) && exit_code == Some(0)
        {
            let text = if stdout.is_empty() { &stderr } else { &stdout };
            let excerpt = normalize_terminal_text(text);
            if !excerpt.is_empty() {
                summary.headline = bounded_text(&excerpt, 1_000);
                if excerpt.len() > 1_000 {
                    summary.truncated = true;
                    summary.inspect_hint = Some(InspectHint::Log);
                }
            }
        }
        let run = self.build_run_summary(
            &queued.request,
            exit_code,
            &exited.termination,
            run_bep_outcome,
            &stdout,
            &stderr,
        );
        if let Some(run) = &run {
            match run.outcome {
                RunOutcome::BuildFailed => {}
                RunOutcome::NotLaunched => {
                    summary.headline = format!("{} was built but not launched", run.target);
                }
                RunOutcome::Succeeded => {
                    summary.headline = format!("{} exited successfully", run.target);
                }
                RunOutcome::ProgramFailed => {
                    summary.headline = run.program_exit_code.map_or_else(
                        || format!("{} terminated without an exit code", run.target),
                        |code| format!("{} exited with code {code}", run.target),
                    );
                }
                RunOutcome::CancelledDuringBuild => {
                    summary.headline = format!("{} was cancelled while building", run.target);
                }
                RunOutcome::CancelledDuringProgram => {
                    summary.headline = format!("{} was cancelled while running", run.target);
                }
                RunOutcome::TimedOutDuringBuild => {
                    summary.headline = format!("{} timed out while building", run.target);
                }
                RunOutcome::TimedOutDuringProgram => {
                    summary.headline = format!("{} timed out while running", run.target);
                }
                RunOutcome::OutputLimitDuringBuild => {
                    summary.headline =
                        format!("{} exceeded the output limit while building", run.target);
                }
                RunOutcome::OutputLimitDuringProgram => {
                    summary.headline =
                        format!("{} exceeded the output limit while running", run.target);
                }
            }
        }
        self.enrich_tests(paths, &queued.request.workspace, &mut summary, &artifacts)
            .await;
        if queued.request.command == BazelCommand::Coverage {
            summary.coverage = self
                .load_coverage(&queued.request.workspace, &artifacts)
                .await;
        }
        let mut custom_headline = None;
        let mut custom_notices = Vec::new();
        if !self.reducers.is_empty() {
            let context = self.reducer_context(
                &queued.request,
                exit_code,
                bazel_wall_ms,
                &stdout,
                &stderr,
                reducer_events,
                reducer_input_truncated,
                &summary,
            );
            let reducers = self.reducers.clone();
            let (next_summary, report) = task::spawn_blocking(move || {
                let mut next_summary = summary;
                let report = reducers.apply(&context, &mut next_summary);
                (next_summary, report)
            })
            .await?;
            summary = next_summary;
            if report.headline_applied {
                custom_headline = Some(summary.headline.clone());
            }
            for name in report.applied {
                let name = self.redactor.redact_bounded(&name, 128);
                tracing::debug!(invocation_id = %queued.request.id, reducer = %name, "applied custom reducer");
            }
            for failure in report.failures {
                let name = self.redactor.redact_bounded(&failure.name, 128);
                let error = self.redactor.redact_bounded(&failure.error, 2 * 1024);
                tracing::warn!(
                    invocation_id = %queued.request.id,
                    reducer = %name,
                    %error,
                    "custom reducer failed; native result retained"
                );
                custom_notices.push(custom_reducer_notice(format!(
                    "Custom reducer {:?} failed; native reducer output was retained",
                    name
                )));
            }
            for collision in report.override_collisions {
                let collision = self.redactor.redact_bounded(&collision, 512);
                tracing::warn!(invocation_id = %queued.request.id, %collision, "custom reducer override collision");
                custom_notices.push(custom_reducer_notice(collision));
            }
        }
        finalize_with_custom_notices(&mut summary, custom_notices);
        if let Some(headline) = custom_headline {
            summary.headline = headline;
        } else if queued.driver.is_aspect() {
            if !summary.success
                && let Some(first) = summary.diagnostics.first()
            {
                summary.headline = format!(
                    "Aspect {} failed: {}",
                    queued.request.command, first.message
                );
            } else if summary.success && command_class == CommandClass::BuildLike {
                summary.headline = format!(
                    "Aspect {} completed successfully in {bazel_wall_ms} ms",
                    queued.request.command
                );
            } else if !summary.success
                && (summary.headline.is_empty() || summary.headline.starts_with("Bazel "))
            {
                summary.headline = format!(
                    "Aspect {} failed with exit code {exit_code:?}",
                    queued.request.command
                );
            }
        }
        if !summary.success && summary.inspect_hint.is_none() {
            let hint = if summary
                .tests
                .iter()
                .any(|test| test.status != TestStatus::Passed && test.test_log_available)
            {
                InspectHint::TestLog
            } else {
                InspectHint::Log
            };
            summary.inspect_hint = Some(hint);
        }
        self.sanitize_summary(queued.request.id, &queued.request.workspace, &mut summary);
        let metrics = InvocationMetrics {
            raw_output_bytes: capture::file_size(&paths.stdout)
                .await
                .saturating_add(capture::file_size(&paths.stderr).await),
            bep_bytes: capture::file_size(&paths.bep).await,
            bep_events: u64::try_from(bep_outcome.event_count).unwrap_or(u64::MAX),
            queue_ms: exited.plan.queue_ms,
            output_base_lock_wait_ms: exited.output_base_lock_wait_ms,
            bazel_wall_ms,
            reduction_ms: duration_millis(reduction_started.elapsed()),
            ..Default::default()
        };
        Ok(ReducedOutcome {
            id: queued.request.id,
            completion: InvocationCompletion {
                state: exited.state,
                termination: exited.termination,
                summary,
                run,
                metrics,
                canonical_arguments,
                artifacts,
            },
        })
    }

    async fn commit_terminal(
        &self,
        commit: TerminalCommit,
    ) -> Result<InvocationRecord, RunnerError> {
        let fallback = commit.recover_without_evidence.then(|| {
            (
                commit.completion.state,
                commit.completion.termination.clone(),
                commit.completion.summary.clone(),
            )
        });
        match self.finish_invocation(commit.id, commit.completion).await {
            Ok(record) => Ok(record),
            Err(error) => {
                if let Ok(record) = self.store.get_invocation_header(commit.id).await
                    && record.state.is_terminal()
                    && record.summary.is_some()
                {
                    return Ok(record.into_record());
                }
                if matches!(&error, StoreError::Io(error) if error.kind() == std::io::ErrorKind::NotFound)
                    && let Some((state, termination, summary)) = fallback
                {
                    return self
                        .transition_invocation(commit.id, state, Some(termination), Some(summary))
                        .await
                        .map_err(Into::into);
                }
                Err(error.into())
            }
        }
    }

    async fn recover_terminal_failure(
        &self,
        context: TerminalContext,
        error: RunnerError,
    ) -> Result<InvocationRecord, RunnerError> {
        let workspace = context.workspace.to_string_lossy();
        let message = self.redactor.redact_bounded(
            &error.to_string().replace(workspace.as_ref(), "<workspace>"),
            1_000,
        );
        tracing::warn!(
            invocation_id = %context.id,
            error = %message,
            "Bazel result post-processing failed"
        );
        if let Ok(record) = self.store.get_invocation_header(context.id).await
            && record.state.is_terminal()
            && record.summary.is_some()
        {
            return Ok(record.into_record());
        }
        let terminal_state = if context.state == InvocationState::Succeeded {
            InvocationState::Failed
        } else {
            context.state
        };
        let commit = TerminalCommit {
            id: context.id,
            completion: InvocationCompletion {
                state: terminal_state,
                termination: context.termination,
                summary: InvocationSummary {
                    success: false,
                    headline: format!("Could not finish processing Bazel results: {message}"),
                    elapsed_ms: context.elapsed_ms,
                    truncated: true,
                    inspect_hint: Some(InspectHint::Log),
                    ..Default::default()
                },
                run: None,
                metrics: InvocationMetrics::default(),
                canonical_arguments: None,
                artifacts: Vec::new(),
            },
            recover_without_evidence: true,
        };
        match self.commit_terminal(commit).await {
            Ok(record) => Ok(record),
            Err(_) => Err(error),
        }
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
                let path = location.path.replace(workspace.as_ref(), "<workspace>");
                let path = path
                    .strip_prefix("<workspace>/")
                    .or_else(|| path.strip_prefix("<workspace>\\"))
                    .or_else(|| path.strip_prefix("<WORKSPACE>/"))
                    .or_else(|| path.strip_prefix("<WORKSPACE>\\"))
                    .unwrap_or(&path);
                location.path = self.redactor.redact_bounded(path, 1_000);
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

    fn build_run_summary(
        &self,
        request: &InvocationRequest,
        exit_code: Option<i32>,
        termination: &Termination,
        bep: RunBepOutcome,
        stdout: &[u8],
        stderr: &[u8],
    ) -> Option<RunSummary> {
        if request.command != BazelCommand::Run {
            return None;
        }
        let launched = bep.exec_request_should_execute == Some(true);
        let outcome = match termination {
            Termination::OutputLimit { .. } if launched => RunOutcome::OutputLimitDuringProgram,
            Termination::OutputLimit { .. } => RunOutcome::OutputLimitDuringBuild,
            Termination::Cancelled if launched => RunOutcome::CancelledDuringProgram,
            Termination::Cancelled => RunOutcome::CancelledDuringBuild,
            Termination::Timeout if launched => RunOutcome::TimedOutDuringProgram,
            Termination::Timeout => RunOutcome::TimedOutDuringBuild,
            _ if bep.build_success == Some(false) => RunOutcome::BuildFailed,
            _ if !launched => RunOutcome::NotLaunched,
            _ if exit_code == Some(0) => RunOutcome::Succeeded,
            _ => RunOutcome::ProgramFailed,
        };
        let program_exit_code = launched.then_some(exit_code).flatten();
        let output_excerpt = if launched {
            self.run_output_excerpt(&request.workspace, stdout, stderr)
        } else {
            Vec::new()
        };
        Some(RunSummary {
            target: self.redactor.redact_bounded(
                request.target.as_deref().unwrap_or("<missing target>"),
                1_000,
            ),
            outcome,
            program_exit_code,
            output_excerpt,
        })
    }

    fn run_output_excerpt(&self, workspace: &Path, stdout: &[u8], stderr: &[u8]) -> Vec<String> {
        let normalized_stdout = normalize_terminal_text(stdout);
        let text = if normalized_stdout.trim().is_empty() {
            normalize_terminal_text(stderr)
        } else {
            normalized_stdout
        };
        let workspace = workspace.to_string_lossy();
        let mut lines = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .rev()
            .take(8)
            .map(|line| {
                self.redactor
                    .redact_bounded(&line.replace(workspace.as_ref(), "<workspace>"), 512)
            })
            .collect::<Vec<_>>();
        lines.reverse();
        lines
    }

    #[allow(clippy::too_many_arguments)]
    fn reducer_context(
        &self,
        request: &InvocationRequest,
        exit_code: Option<i32>,
        elapsed_ms: u64,
        stdout: &[u8],
        stderr: &[u8],
        mut events: Vec<bazel_mcp_reducer::ReducerEvent>,
        mut input_truncated: bool,
        summary: &bazel_mcp_types::InvocationSummary,
    ) -> ReducerContext {
        let maximum_input = self.config.starlark_reducers.limits.max_input_bytes;
        let workspace = request.workspace.to_string_lossy();
        let sanitize = |value: &str, maximum_bytes| {
            self.redactor.redact_bounded(
                &value.replace(workspace.as_ref(), "<workspace>"),
                maximum_bytes,
            )
        };
        let stdout = normalize_terminal_text(stdout);
        let stderr = normalize_terminal_text(stderr);
        let (stdout_limit, stderr_limit) = match (stdout.is_empty(), stderr.is_empty()) {
            (false, true) => (maximum_input, 0),
            (true, false) => (0, maximum_input),
            _ => (
                maximum_input / 2,
                maximum_input.saturating_sub(maximum_input / 2),
            ),
        };
        input_truncated |= stdout.len() > stdout_limit || stderr.len() > stderr_limit;
        let stdout = sanitize(&stdout, stdout_limit);
        let stderr = sanitize(&stderr, stderr_limit);
        for event in &mut events {
            event.label = event
                .label
                .as_deref()
                .map(|value| sanitize(value, 4 * 1024));
            event.target_kind = event
                .target_kind
                .as_deref()
                .map(|value| sanitize(value, 4 * 1024));
            event.action_type = event
                .action_type
                .as_deref()
                .map(|value| sanitize(value, 4 * 1024));
            event.message = event
                .message
                .as_deref()
                .map(|value| sanitize(value, 64 * 1024));
        }
        const MAX_ARGUMENTS: usize = 1_024;
        let raw_arguments = request
            .startup_arguments
            .iter()
            .chain(request.arguments.iter());
        let arguments = raw_arguments
            .take(MAX_ARGUMENTS)
            .map(|value| sanitize(value, 4 * 1024))
            .collect::<Vec<_>>();
        input_truncated |= request
            .startup_arguments
            .len()
            .saturating_add(request.arguments.len())
            > MAX_ARGUMENTS;
        let mut baseline = summary.clone();
        finalize_diagnostics(
            &mut baseline,
            Budget {
                max_bytes: maximum_input / 4,
                max_items: self.config.starlark_reducers.limits.max_output_items,
            },
        );
        self.sanitize_summary(request.id, &request.workspace, &mut baseline);
        ReducerContext {
            api_version: REDUCER_API_VERSION,
            command: request.command.as_str().to_owned(),
            arguments,
            exit_code,
            elapsed_ms,
            stdout,
            stderr,
            events,
            input_truncated,
            baseline,
        }
    }

    pub(crate) async fn finish_cancelled(
        &self,
        id: InvocationId,
    ) -> Result<InvocationRecord, RunnerError> {
        let current = self.store.get_invocation(id).await?;
        if current.state.is_terminal() {
            return Ok(current);
        }
        match self
            .transition_invocation(
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
}

#[cfg(unix)]
async fn probe_bazel_server_pid(
    executable: &Path,
    workspace: &Path,
    startup_arguments: &[String],
    config: &RunnerConfig,
    cancellation: CancellationToken,
) -> io::Result<u32> {
    let mut command = Command::new(executable);
    command
        .current_dir(workspace)
        .env_clear()
        .envs(filtered_environment(&config.policy));
    if let Some(output_user_root) = &config.output_user_root {
        command
            .arg(format!("--output_user_root={}", output_user_root.display()))
            .arg(format!(
                "--max_idle_secs={}",
                config.isolated_bazel_server_idle_timeout.as_secs()
            ));
    }
    command
        .args(startup_arguments)
        .arg("info")
        .arg("server_pid")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);

    let output = tokio::select! {
        result = tokio::time::timeout(config.version_check_timeout, command.output()) => {
            result.map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "Bazel server PID probe timed out"))??
        }
        () = cancellation.cancelled() => {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "Bazel server PID probe cancelled"));
        }
    };
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "Bazel server PID probe exited with {:?}",
            output.status.code()
        )));
    }
    let stdout = String::from_utf8(output.stdout).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidData, "Bazel server PID was not UTF-8")
    })?;
    stdout
        .lines()
        .find_map(|line| {
            let value = line
                .trim()
                .strip_prefix("server_pid:")
                .map_or_else(|| line.trim(), str::trim);
            value.parse::<u32>().ok()
        })
        .filter(|pid| *pid > 0 && *pid <= i32::MAX as u32)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Bazel server PID probe returned no valid PID",
            )
        })
}

fn custom_reducer_notice(message: String) -> Diagnostic {
    Diagnostic {
        severity: Severity::Note,
        category: DiagnosticCategory::Bazel,
        message,
        location: None,
        target: None,
        action: None,
        repetition_count: 1,
    }
}

fn finalize_with_custom_notices(
    summary: &mut bazel_mcp_types::InvocationSummary,
    notices: Vec<Diagnostic>,
) {
    if notices.is_empty() {
        finalize_diagnostics(summary, Budget::result_default());
        return;
    }
    let mut notice_summary = bazel_mcp_types::InvocationSummary {
        success: true,
        diagnostics: notices,
        ..Default::default()
    };
    finalize_diagnostics(
        &mut notice_summary,
        Budget {
            max_bytes: 1024,
            max_items: 4,
        },
    );
    let notices = notice_summary.diagnostics;
    let notice_bytes = notices
        .iter()
        .map(|notice| notice.message.len())
        .sum::<usize>();
    let budget = Budget::result_default();
    finalize_diagnostics(
        summary,
        Budget {
            max_bytes: budget.max_bytes.saturating_sub(notice_bytes),
            max_items: budget.max_items.saturating_sub(notices.len()),
        },
    );
    summary.diagnostics.extend(notices);
    if notice_summary.truncated {
        summary.truncated = true;
        summary.inspect_hint = Some(InspectHint::Diagnostics);
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

fn captures_bep(command_class: CommandClass) -> bool {
    matches!(command_class, CommandClass::BuildLike | CommandClass::Run)
}

async fn wait_for_run_output_limit(stdout: &Path, stderr: &Path, maximum_bytes: u64) {
    loop {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let bytes = capture::file_size(stdout)
            .await
            .saturating_add(capture::file_size(stderr).await);
        if bytes > maximum_bytes {
            return;
        }
    }
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

pub(crate) fn cancelled_summary() -> bazel_mcp_types::InvocationSummary {
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
        inspect_hint: Some(InspectHint::Log),
        ..Default::default()
    }
}
