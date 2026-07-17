//! Failed-test artifact acquisition and durable snapshot orchestration.

use std::path::Path;

use bazel_mcp_reducer::{
    TestEvidenceInput, TestEvidenceReducer, normalize_terminal_text, parse_test_xml,
};
use bazel_mcp_store::InvocationPaths;
use bazel_mcp_types::{ArtifactKind, DiagnosticCategory, TestStatus};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::{
    evidence::create_private_file,
    inspection::EvidenceRecord,
    service::{InvocationService, bounded_text},
};

impl InvocationService {
    pub(crate) async fn enrich_tests(
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

        let (raw, evidence) = tokio::join!(
            create_private_file(&raw_temporary),
            create_private_file(&evidence_temporary)
        );
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
            let mut reducer = TestEvidenceReducer::new(TestEvidenceInput { label: &test.label });
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
                        reducer.observe_line(&text);
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
                reducer.finish_log(complete);
                if complete {
                    copied_for_test = true;
                    copied_any = true;
                }
            }
            if copied_for_test {
                test.test_log_available = true;
                test.test_log_unavailable_reason = None;
                let reduced = reducer.finish();
                if reduced.cases.is_empty() {
                    diagnostics.extend(reduced.diagnostics);
                } else {
                    let previous_cases = std::mem::take(&mut test.cases);
                    let mut cases = reduced.cases;
                    for case in previous_cases {
                        if cases.len() >= 20 {
                            break;
                        }
                        if !cases.iter().any(|current| {
                            current.name == case.name && current.message == case.message
                        }) {
                            cases.push(case);
                        }
                    }
                    test.cases = cases;
                    diagnostics.extend(reduced.diagnostics);
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
            for diagnostic in &diagnostics {
                summary.diagnostics.retain(|existing| {
                    !((existing.category == DiagnosticCategory::Compilation
                        || (existing.category == diagnostic.category && existing.target.is_none()))
                        && existing.message == diagnostic.message
                        && (existing.location == diagnostic.location
                            || existing.location.is_none()))
                });
            }
            summary.diagnostics.extend(diagnostics);
        } else {
            let _ = tokio::fs::remove_file(&raw_temporary).await;
            let _ = tokio::fs::remove_file(&evidence_temporary).await;
            if copied_any {
                set_test_log_unavailable(summary, "test_log_snapshot_failed");
            }
        }
    }
}

pub(crate) fn artifact_matches_test(artifact: &bazel_mcp_types::Artifact, label: &str) -> bool {
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

pub(crate) fn set_test_log_unavailable(
    summary: &mut bazel_mcp_types::InvocationSummary,
    reason: &str,
) {
    for test in summary
        .tests
        .iter_mut()
        .filter(|test| test.status != TestStatus::Passed)
    {
        test.test_log_available = false;
        test.test_log_unavailable_reason = Some(reason.to_owned());
    }
}
