//! Bazel process execution and terminal summary finalization.

use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    path::Path,
    process::ExitStatus,
    sync::Arc,
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::io;

use bazel_mcp_policy::filtered_environment;
use bazel_mcp_reducer::{
    Budget, REDUCER_API_VERSION, ReducerContext, StreamReductionOutput, finalize_diagnostics,
    normalize_terminal_text,
};
use bazel_mcp_store::{InvocationCompletion, InvocationPaths, StoreError};
use bazel_mcp_types::{
    BazelCommand, CommandClass, Diagnostic, DiagnosticCategory, InspectHint, InvocationId,
    InvocationMetrics, InvocationRecord, InvocationRequest, InvocationState, PageRequest, Severity,
    Termination, TestStatus,
};
use tokio::{process::Command, task};
use tokio_util::sync::CancellationToken;

use crate::{
    cancel::{ProcessGroupGuard, terminate_child},
    capture,
    inspection::should_persist_failure_evidence,
    output_base_lock::{NativeOutputBaseWaitObserver, OutputBaseWaitStatus},
    service::{
        BES_COMPLETION_GRACE, BepTransport, COMPLETE_BEP_LOG_LIMIT, FALLBACK_LOG_LIMIT,
        InvocationService, RunnerError, bounded_text,
    },
};

#[cfg(unix)]
use crate::service::RunnerConfig;

impl InvocationService {
    pub(crate) async fn execute(
        &self,
        queued: &InvocationRecord,
        paths: &InvocationPaths,
        executable: &Path,
        cancellation: CancellationToken,
        explicit_output_base: Option<&Path>,
        output_base_wait: Arc<OutputBaseWaitStatus>,
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
                    .transition_invocation(
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
        let native_output_base_wait = NativeOutputBaseWaitObserver::start(
            paths.stdout.clone(),
            paths.stderr.clone(),
            paths.bep.clone(),
            explicit_output_base.map(Path::to_owned),
            output_base_wait.clone(),
        )
        .await;
        #[cfg(unix)]
        let mut prepared_fifo = if queued.request.command.class() == CommandClass::BuildLike
            && self.config.bep_transport == BepTransport::Fifo
        {
            match capture::PreparedFifoBepCapture::prepare(&paths.bep) {
                Ok(prepared) => match probe_bazel_server_pid(
                    executable,
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
        if queued.request.command.class() == CommandClass::BuildLike
            && self.config.bep_transport == BepTransport::Fifo
        {
            tracing::debug!(
                invocation_id = %queued.request.id,
                "BEP FIFO transport is unavailable on this platform; using file tail"
            );
        }
        if cancellation.is_cancelled() {
            return self.finish_cancelled(queued.request.id).await;
        }
        let extension_limits = (!self.reducers.is_empty()).then_some({
            (
                self.config.starlark_reducers.limits.max_events,
                self.config.starlark_reducers.limits.max_input_bytes,
            )
        });
        let bes_bep = if queued.request.command.class() == CommandClass::BuildLike {
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
            command.arg(format!("--invocation_id={}", queued.request.id));
            match self.config.bep_transport {
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
                    .transition_invocation(
                        queued.request.id,
                        InvocationState::Failed,
                        Some(Termination::SpawnFailure { message }),
                        Some(summary),
                    )
                    .await
                    .map_err(Into::into);
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
        let mut process_group = ProcessGroupGuard::for_child(&child);
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
            if self
                .store
                .get_invocation_header(queued.request.id)
                .await
                .is_ok_and(|record| !record.state.is_terminal())
            {
                let summary = bazel_mcp_types::InvocationSummary {
                    success: false,
                    headline: format!("Could not record Bazel execution state: {message}"),
                    truncated: true,
                    inspect_hint: Some(InspectHint::Log),
                    ..Default::default()
                };
                let _ = self
                    .transition_invocation(
                        queued.request.id,
                        InvocationState::Failed,
                        Some(Termination::Interrupted),
                        Some(summary),
                    )
                    .await;
            }
            if let Ok(record) = self.store.get_invocation_header(queued.request.id).await
                && record.state.is_terminal()
                && record.summary.is_some()
            {
                return Ok(record.into_record());
            }
            return Err(error.into());
        }
        #[cfg(unix)]
        let incremental_bep = fifo_bep.or(bes_bep).or_else(|| {
            (queued.request.command.class() == CommandClass::BuildLike
                && self.config.bep_transport != BepTransport::Bes)
                .then(|| {
                    capture::LiveBepCapture::Tail(capture::IncrementalBepCapture::start(
                        paths.bep.clone(),
                        extension_limits,
                    ))
                })
        });
        #[cfg(not(unix))]
        let incremental_bep = bes_bep.or_else(|| {
            (queued.request.command.class() == CommandClass::BuildLike
                && self.config.bep_transport != BepTransport::Bes)
                .then(|| {
                    capture::LiveBepCapture::Tail(capture::IncrementalBepCapture::start(
                        paths.bep.clone(),
                        extension_limits,
                    ))
                })
        });
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
        native_output_base_wait.finish().await;
        let output_base_wait_snapshot = output_base_wait.snapshot();
        let bazel_wall_ms = duration_millis(started.elapsed());
        let postprocess: Result<InvocationRecord, RunnerError> = async {
            let reduction_started = Instant::now();
            let incremental_reduction = match incremental_bep {
                Some(capture) => {
                    let reduction = capture.finish(BES_COMPLETION_GRACE).await?;
                    tracing::debug!(
                        invocation_id = %queued.request.id,
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
            let (query_row_count, query_sample) =
                if queued.request.command.class() == CommandClass::Query {
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
                None => capture::reduce_bep(paths.bep.clone(), extension_limits).await?,
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
                summary.inspect_hint =
                    (query_row_count > 0).then_some(InspectHint::QueryResults);
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
                        summary.inspect_hint = Some(InspectHint::Log);
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
                queue_ms,
                output_base_lock_wait_ms: output_base_wait_snapshot.elapsed_ms,
                bazel_wall_ms,
                reduction_ms: duration_millis(reduction_started.elapsed()),
                ..Default::default()
            };
            self.finish_invocation(
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
                .get_invocation_header(queued.request.id)
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
                    inspect_hint: Some(InspectHint::Log),
                    ..Default::default()
                };
                let _ = self
                    .transition_invocation(
                        queued.request.id,
                        terminal_state,
                        Some(termination),
                        Some(summary),
                    )
                    .await;
            }
            if let Ok(record) = self.store.get_invocation_header(queued.request.id).await
                && record.state.is_terminal()
                && record.summary.is_some()
            {
                return Ok(record.into_record());
            }
        }
        postprocess
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
pub(crate) async fn probe_bazel_server_pid(
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

pub(crate) fn custom_reducer_notice(message: String) -> Diagnostic {
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

pub(crate) fn finalize_with_custom_notices(
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

pub(crate) fn finish_from_status(
    status: ExitStatus,
) -> (Option<ExitStatus>, Termination, InvocationState) {
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

pub(crate) fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

pub(crate) fn cancelled_summary() -> bazel_mcp_types::InvocationSummary {
    bazel_mcp_types::InvocationSummary {
        success: false,
        headline: "Bazel invocation was cancelled before starting".to_owned(),
        ..Default::default()
    }
}

pub(crate) fn fallback_summary(
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
