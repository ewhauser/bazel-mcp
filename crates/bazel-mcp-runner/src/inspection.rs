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
    ArtifactKind, BazelCommand, CommandClass, InvocationId, InvocationState, PageRequest,
    Termination,
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::{
    capture,
    evidence::write_private_atomic,
    service::{InvocationService, RunnerError, bounded_text},
};

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

impl InvocationService {
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
                    .page_query_rows_mapped_into(
                        id,
                        request.filter.as_deref(),
                        page_request,
                        move |value, output| {
                            redactor.redact_bounded_into(value, 4 * 1024, output);
                        },
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

pub(crate) fn page_evidence_records(
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

fn invocation_ledger_row(record: &InvocationHeader) -> serde_json::Value {
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
