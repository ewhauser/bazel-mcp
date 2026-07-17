//! Bounded inspection requests, pagination, and response shaping.

use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use bazel_mcp_reducer::normalize_terminal_text;
use bazel_mcp_store::{InvocationHeader, InvocationPaths, StoreError};
use bazel_mcp_types::{
    ArtifactKind, BazelCommand, CommandClass, InspectCoverageItem, InspectCoverageSummary,
    InspectCoverageUnavailable, InspectMetrics, InspectPayload, InspectResult, InspectSummary,
    InspectView, InvocationId, InvocationLedgerEntry, InvocationState, PageRequest, Termination,
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::{
    capture,
    evidence::write_private_atomic,
    service::{InvocationService, RunnerError, bounded_text},
};

#[derive(Clone, Debug)]
pub struct InspectRequest {
    pub invocation_id: Option<InvocationId>,
    pub workspace: Option<PathBuf>,
    pub state: Option<InvocationState>,
    pub command: Option<BazelCommand>,
    pub view: InspectView,
    pub cursor: Option<String>,
    pub filter: Option<String>,
    pub item_limit: u32,
    pub scan_limit: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct LogCursor {
    invocation_id: InvocationId,
    view: InspectView,
    pub(crate) next_record: u64,
    filter: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct EvidenceRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) label: Option<String>,
    pub(crate) text: String,
}

pub(crate) struct EvidencePage {
    pub(crate) items: Vec<String>,
    pub(crate) item_cursors: Vec<String>,
    pub(crate) truncated: bool,
    pub(crate) next_cursor: Option<String>,
}

impl LogCursor {
    pub(crate) fn encode(&self) -> Result<String, RunnerError> {
        Ok(URL_SAFE_NO_PAD.encode(serde_json::to_vec(self)?))
    }

    fn decode(value: &str) -> Result<Self, RunnerError> {
        let raw = URL_SAFE_NO_PAD
            .decode(value)
            .map_err(|_| RunnerError::InvalidOffset)?;
        serde_json::from_slice(&raw).map_err(|_| RunnerError::InvalidOffset)
    }

    pub(crate) fn decode_for(
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

impl InvocationService {
    pub async fn inspect(&self, request: InspectRequest) -> Result<InspectResult, RunnerError> {
        if request.view == InspectView::Invocations {
            let page = self
                .store
                .list_invocations(
                    request.workspace.as_deref(),
                    request.state,
                    request.command.as_ref(),
                    PageRequest {
                        cursor: request.cursor.clone(),
                        item_limit: request.item_limit.clamp(1, 100),
                        scan_limit: request.scan_limit,
                    },
                )
                .await?;
            return Ok(InspectResult::new(
                None,
                InspectPayload::Invocations(page.items.iter().map(invocation_ledger_row).collect()),
                page.total_count,
                page.filtered_count,
                page.next_cursor,
                page.truncated,
                page.item_cursors,
            )
            .with_start_cursor(request.cursor));
        }

        let id = request.invocation_id.ok_or(StoreError::InvalidCursor)?;
        let record = self.store.get_invocation(id).await?;
        let page_request = PageRequest {
            cursor: request.cursor.clone(),
            item_limit: request.item_limit.clamp(1, 100),
            scan_limit: request.scan_limit,
        };
        let paths = self.store.paths_for(&record);
        let result = match request.view {
            InspectView::Summary => {
                let items = record.summary.as_ref().map_or_else(Vec::new, |summary| {
                    vec![InspectSummary {
                        success: summary.success,
                        headline: summary.headline.clone(),
                        targets: summary.target_counts.clone(),
                        tests: summary.test_counts.clone(),
                        diagnostics: summary.diagnostics.clone(),
                        coverage: summary.coverage.as_ref().map(|coverage| {
                            InspectCoverageSummary {
                                lines_found: coverage.lines_found,
                                lines_hit: coverage.lines_hit,
                                coverage_percent: coverage.coverage_percent,
                            }
                        }),
                        query_result_count: summary.query_result_count,
                        query_sample: summary.query_sample.clone(),
                        elapsed_ms: summary.elapsed_ms,
                        truncated: summary.truncated,
                        inspect_hint: summary.inspect_hint,
                    }]
                });
                InspectResult::new(
                    Some(id),
                    InspectPayload::Summary(items),
                    Some(u64::from(record.summary.is_some())),
                    Some(u64::from(record.summary.is_some())),
                    None,
                    record
                        .summary
                        .as_ref()
                        .is_some_and(|summary| summary.truncated),
                    Vec::new(),
                )
            }
            InspectView::Metrics => InspectResult::new(
                Some(id),
                InspectPayload::Metrics(vec![InspectMetrics {
                    state: record.state,
                    requested_at_ms: record.request.requested_at_ms,
                    started_at_ms: record.started_at_ms,
                    finished_at_ms: record.finished_at_ms,
                    termination: record.termination.clone(),
                    metrics: record.metrics.clone(),
                }]),
                Some(1),
                Some(1),
                None,
                false,
                Vec::new(),
            ),
            InspectView::Diagnostics => {
                let page = self
                    .store
                    .page_diagnostics(id, request.filter.as_deref(), page_request)
                    .await?;
                InspectResult::new(
                    Some(id),
                    InspectPayload::Diagnostics(page.items),
                    page.total_count,
                    page.filtered_count,
                    page.next_cursor,
                    page.truncated,
                    page.item_cursors,
                )
            }
            InspectView::Tests => {
                let page = self
                    .store
                    .page_tests(id, request.filter.as_deref(), page_request)
                    .await?;
                InspectResult::new(
                    Some(id),
                    InspectPayload::Tests(page.items),
                    page.total_count,
                    page.filtered_count,
                    page.next_cursor,
                    page.truncated,
                    page.item_cursors,
                )
            }
            InspectView::Artifacts => {
                let page = self
                    .store
                    .page_artifacts(id, request.filter.as_deref(), page_request)
                    .await?;
                InspectResult::new(
                    Some(id),
                    InspectPayload::Artifacts(page.items),
                    page.total_count,
                    page.filtered_count,
                    page.next_cursor,
                    page.truncated,
                    page.item_cursors,
                )
            }
            InspectView::QueryResults => {
                let redactor = self.redactor.clone();
                let page = self
                    .store
                    .page_query_rows_mapped_into(
                        id,
                        request.filter.as_deref(),
                        page_request,
                        move |value, output| {
                            redactor.redact_bounded_into(value, 4 * 1024, output);
                        },
                    )
                    .await?;
                InspectResult::new(
                    Some(id),
                    InspectPayload::QueryResults(page.items),
                    page.total_count,
                    page.filtered_count,
                    page.next_cursor,
                    page.truncated,
                    page.item_cursors,
                )
            }
            InspectView::Coverage => {
                let page = self
                    .store
                    .page_coverage(id, request.filter.as_deref(), page_request)
                    .await?;
                let mut items = page
                    .items
                    .into_iter()
                    .map(InspectCoverageItem::File)
                    .collect::<Vec<_>>();
                let mut total_count = page.total_count;
                let mut filtered_count = page.filtered_count;
                let mut next_cursor = page.next_cursor;
                let mut truncated = page.truncated;
                let mut item_cursors = page.item_cursors;
                if items.is_empty() {
                    let artifacts = self
                        .store
                        .page_artifacts(
                            id,
                            request.filter.as_deref(),
                            PageRequest {
                                cursor: None,
                                item_limit: request.item_limit,
                                scan_limit: request.scan_limit,
                            },
                        )
                        .await?;
                    items.clear();
                    item_cursors.clear();
                    for (artifact, cursor) in
                        artifacts.items.into_iter().zip(artifacts.item_cursors)
                    {
                        if artifact.kind == ArtifactKind::Coverage && !artifact.locally_available {
                            items.push(InspectCoverageItem::Unavailable(
                                InspectCoverageUnavailable {
                                    availability_reason: "remote_artifact_unavailable",
                                    artifact: Some(artifact),
                                },
                            ));
                            item_cursors.push(cursor);
                        }
                    }
                    if items.is_empty() {
                        items.push(InspectCoverageItem::Unavailable(
                            InspectCoverageUnavailable {
                                availability_reason: "coverage_artifact_not_found",
                                artifact: None,
                            },
                        ));
                    }
                    total_count = Some(items.len() as u64);
                    filtered_count = total_count;
                    next_cursor = artifacts.next_cursor;
                    truncated = artifacts.truncated;
                }
                InspectResult::new(
                    Some(id),
                    InspectPayload::Coverage(items),
                    total_count,
                    filtered_count,
                    next_cursor,
                    truncated,
                    item_cursors,
                )
            }
            InspectView::Log => {
                let page = self
                    .read_evidence_page(&paths.evidence, &paths, &request)
                    .await?;
                InspectResult::new(
                    Some(id),
                    InspectPayload::Log(page.items),
                    None,
                    None,
                    page.next_cursor,
                    page.truncated,
                    page.item_cursors,
                )
            }
            InspectView::TestLog => {
                let page = if paths.test_log_evidence.exists() {
                    self.read_evidence_page(&paths.test_log_evidence, &paths, &request)
                        .await?
                } else {
                    self.read_test_log_unavailable_page(id, &request).await?
                };
                InspectResult::new(
                    Some(id),
                    InspectPayload::TestLog(page.items),
                    None,
                    None,
                    page.next_cursor,
                    page.truncated,
                    page.item_cursors,
                )
            }
            InspectView::Invocations => unreachable!("handled above"),
        };
        Ok(result.with_start_cursor(request.cursor))
    }

    pub(crate) async fn persist_failure_evidence(
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
        write_private_atomic(&paths.evidence, bytes).await?;
        Ok(())
    }

    async fn read_evidence_page(
        &self,
        path: &Path,
        paths: &InvocationPaths,
        request: &InspectRequest,
    ) -> Result<EvidencePage, RunnerError> {
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
    ) -> Result<EvidencePage, RunnerError> {
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
        let maximum_items = usize::try_from(request.item_limit.clamp(1, 100)).unwrap_or(100);
        let maximum_scanned =
            usize::try_from(request.scan_limit.clamp(request.item_limit.max(1), 10_000))
                .unwrap_or(10_000);
        let filter = request.filter.as_deref().map(str::to_ascii_lowercase);
        let mut items = Vec::new();
        let mut item_cursors = Vec::new();
        let mut reason_index = 0_u64;
        let mut next_record = start;
        let mut scanned = 0_usize;
        let mut storage_cursor = None;

        loop {
            let page = self
                .store
                .page_tests(invocation_id, None, PageRequest::new(storage_cursor, 100))
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
                if scanned == maximum_scanned {
                    let next_cursor = LogCursor {
                        invocation_id,
                        view: request.view,
                        next_record,
                        filter: request.filter.clone(),
                    }
                    .encode()?;
                    return Ok(EvidencePage {
                        items,
                        item_cursors,
                        truncated: true,
                        next_cursor: Some(next_cursor),
                    });
                }
                let text = self
                    .redactor
                    .redact_bounded(&format!("{}: {reason}", test.label), 4 * 1024);
                let matches = filter
                    .as_ref()
                    .is_none_or(|filter| text.to_ascii_lowercase().contains(filter));
                if matches && items.len() == maximum_items {
                    let next_cursor = LogCursor {
                        invocation_id,
                        view: request.view,
                        next_record: index,
                        filter: request.filter.clone(),
                    }
                    .encode()?;
                    return Ok(EvidencePage {
                        items,
                        item_cursors,
                        truncated: true,
                        next_cursor: Some(next_cursor),
                    });
                }
                scanned = scanned.saturating_add(1);
                next_record = index.saturating_add(1);
                if matches {
                    let cursor = LogCursor {
                        invocation_id,
                        view: request.view,
                        next_record,
                        filter: request.filter.clone(),
                    }
                    .encode()?;
                    items.push(text);
                    item_cursors.push(cursor);
                }
            }
            if !page.truncated {
                return Ok(EvidencePage {
                    items,
                    item_cursors,
                    truncated: false,
                    next_cursor: None,
                });
            }
            storage_cursor = page.next_cursor;
        }
    }
}

pub(crate) fn page_evidence_records(
    records: impl Iterator<Item = EvidenceRecord>,
    start: u64,
    request: &InspectRequest,
    invocation_id: InvocationId,
) -> Result<EvidencePage, RunnerError> {
    let maximum_items = usize::try_from(request.item_limit.clamp(1, 100)).unwrap_or(100);
    let maximum_scanned =
        usize::try_from(request.scan_limit.clamp(request.item_limit.max(1), 10_000))
            .unwrap_or(10_000);
    let filter = request.filter.as_deref().map(str::to_ascii_lowercase);
    let mut items = Vec::new();
    let mut item_cursors = Vec::new();
    let mut next_record = start;
    let mut scanned = 0_usize;
    let mut truncated = false;
    for (index, record) in records.enumerate() {
        let index = u64::try_from(index).unwrap_or(u64::MAX);
        if index < start {
            continue;
        }
        if scanned == maximum_scanned {
            truncated = true;
            break;
        }
        let matches = filter.as_ref().is_none_or(|filter| {
            record.text.to_ascii_lowercase().contains(filter)
                || record
                    .label
                    .as_deref()
                    .is_some_and(|label| label.to_ascii_lowercase().contains(filter))
        });
        if matches && items.len() == maximum_items {
            next_record = index;
            truncated = true;
            break;
        }
        scanned = scanned.saturating_add(1);
        next_record = index.saturating_add(1);
        if matches {
            let cursor = LogCursor {
                invocation_id,
                view: request.view,
                next_record,
                filter: request.filter.clone(),
            }
            .encode()?;
            items.push(record.text);
            item_cursors.push(cursor);
        }
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
    Ok(EvidencePage {
        items,
        item_cursors,
        truncated,
        next_cursor,
    })
}

pub(crate) fn should_persist_failure_evidence(command: &BazelCommand, failed: bool) -> bool {
    failed || command.class() != CommandClass::Query
}

pub(crate) fn failure_evidence_records(
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
    failure_evidence_priority(line).is_some()
}

fn failure_evidence_priority(line: &str) -> Option<u8> {
    let line = line.trim();
    let lower = line.to_ascii_lowercase();
    let base = lower
        .split_once(" [repeated ")
        .map_or(lower.as_str(), |(base, _)| base);
    if matches!(base, "failure:" | "failures:")
        || (line.starts_with("test ") && base.ends_with(" ... ok"))
    {
        return None;
    }
    if lower.contains("root_cause")
        || lower.contains("panicked at")
        || (lower.contains("assertion") && lower.contains(" failed"))
    {
        Some(0)
    } else if lower.contains("error:")
        || lower.starts_with("error ")
        || lower.contains("fatal:")
        || lower.contains("no such target")
        || lower.contains("no such package")
        || lower.contains("undefined reference")
        || lower.contains("missing strict dependencies")
        || (lower.contains(".go: import of \"") && lower.ends_with('"'))
    {
        Some(1)
    } else if lower.contains("failed:")
        || lower.contains("failure")
        || lower.starts_with("test result: failed")
        || (line.starts_with("test ") && line.ends_with(" ... FAILED"))
    {
        Some(2)
    } else {
        None
    }
}

fn invocation_ledger_row(record: &InvocationHeader) -> InvocationLedgerEntry {
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
    InvocationLedgerEntry {
        invocation_id: record.request.id,
        workspace: bounded_text(&record.request.workspace.to_string_lossy(), 256),
        state: record.state,
        command: record.request.command.clone(),
        arguments,
        arguments_truncated: record.request.arguments.len() > 3,
        requested_at_ms: record.request.requested_at_ms,
        finished_at_ms: record.finished_at_ms,
        exit_code,
        duration_ms: record.metrics.bazel_wall_ms,
        headline: record
            .summary
            .as_ref()
            .map(|summary| bounded_text(&summary.headline, 256)),
        targets: record
            .summary
            .as_ref()
            .map(|summary| summary.target_counts.clone()),
        tests: record
            .summary
            .as_ref()
            .map(|summary| summary.test_counts.clone()),
        raw_output_bytes: record.metrics.raw_output_bytes,
        model_visible_bytes: record.metrics.model_visible_bytes,
        inspect_calls: record.metrics.inspect_calls,
    }
}
