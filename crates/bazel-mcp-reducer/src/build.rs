use bazel_mcp_bep::{
    BepEvent, decode_event_id,
    view::{BuildEventIdView, FileView, NamedSetOfFilesView, build_event, build_event_id, file},
};
use bazel_mcp_types::{
    Artifact, ArtifactKind, Diagnostic, DiagnosticCategory, InvocationSummary, Severity,
    TargetCounts, TargetResult, TestCounts, TestResult, TestStatus,
};
use std::collections::{BTreeMap, BTreeSet};

use crate::{Budget, deduplicate_lines, normalize_terminal_text};

pub struct ReductionInput<'a> {
    pub events: &'a [BepEvent],
    pub stdout: &'a [u8],
    pub stderr: &'a [u8],
    pub exit_code: Option<i32>,
    pub elapsed_ms: u64,
    pub budget: Budget,
}

const STREAM_MAX_ITEMS: usize = 250_000;
const STREAM_MAX_RETAINED_BYTES: usize = 64 * 1024 * 1024;

/// Bounded state retained while BEP frames are decoded one at a time.
///
/// This keeps only reducer-relevant owned fields; protobuf frames are dropped
/// immediately after `observe` returns.
#[derive(Default)]
pub struct BepAccumulator {
    diagnostics: Vec<Diagnostic>,
    targets: Vec<TargetResult>,
    tests: Vec<TestResult>,
    named_sets: BTreeMap<String, OwnedNamedSet>,
    artifact_roots: Vec<String>,
    direct_artifacts: Vec<Artifact>,
    canonical_arguments: Option<Vec<String>>,
    retained_items: usize,
    retained_bytes: usize,
    truncated: bool,
}

#[derive(Default)]
struct OwnedNamedSet {
    files: Vec<Artifact>,
    children: Vec<String>,
}

pub struct StreamReductionOutput {
    pub summary: InvocationSummary,
    pub artifacts: Vec<Artifact>,
    pub canonical_arguments: Option<Vec<String>>,
}

impl BepAccumulator {
    pub fn observe(&mut self, event: BepEvent) {
        let event = event.view();
        let id = decode_event_id(event.id).ok();
        match event.payload.as_ref() {
            Some(build_event::Payload::Aborted(aborted)) => {
                let diagnostic = Diagnostic {
                    severity: Severity::Error,
                    category: abort_category(aborted.reason),
                    message: bounded_text(aborted.description, 64 * 1024),
                    location: None,
                    target: label_from_id(id.as_ref()),
                    action: None,
                    repetition_count: 1,
                };
                let bytes =
                    diagnostic.message.len() + diagnostic.target.as_ref().map_or(0, String::len);
                if self.reserve(1, bytes) {
                    self.diagnostics.push(diagnostic);
                }
            }
            Some(build_event::Payload::Action(action)) if !action.success => {
                let diagnostic = Diagnostic {
                    severity: Severity::Error,
                    category: DiagnosticCategory::Action,
                    message: format!(
                        "{} action failed with exit code {}",
                        if action.r#type.is_empty() {
                            "Bazel"
                        } else {
                            action.r#type
                        },
                        action.exit_code
                    ),
                    location: None,
                    target: label_from_id(id.as_ref()).or_else(|| nonempty(action.label)),
                    action: nonempty(action.r#type),
                    repetition_count: 1,
                };
                let bytes = diagnostic.message.len()
                    + diagnostic.target.as_ref().map_or(0, String::len)
                    + diagnostic.action.as_ref().map_or(0, String::len);
                if self.reserve(1, bytes) {
                    self.diagnostics.push(diagnostic);
                }
                if let Some(output) = action.primary_output.as_option()
                    && let Some(artifact) = file_artifact(output)
                {
                    self.push_direct_artifact(artifact);
                }
            }
            Some(build_event::Payload::Completed(completed)) => {
                let target = TargetResult {
                    label: label_from_id(id.as_ref()).unwrap_or_else(|| "<unknown target>".into()),
                    success: completed.success,
                };
                if self.reserve(1, target.label.len()) {
                    self.targets.push(target);
                }
                for group in &completed.output_group {
                    for set in &group.file_sets {
                        self.push_root(set.id);
                    }
                    for file in &group.inline_files {
                        if let Some(artifact) = file_artifact(file) {
                            self.push_direct_artifact(artifact);
                        }
                    }
                }
                for file in completed
                    .important_output
                    .iter()
                    .chain(completed.directory_output.iter())
                {
                    if let Some(artifact) = file_artifact(file) {
                        self.push_direct_artifact(artifact);
                    }
                }
            }
            Some(build_event::Payload::TestSummary(summary)) => {
                let label = label_from_id(id.as_ref()).unwrap_or_else(|| "<unknown test>".into());
                let status = test_status(summary.overall_status);
                if let Some(diagnostic) = test_outcome_diagnostic(&label, status) {
                    let bytes = diagnostic.message.len()
                        + diagnostic.target.as_ref().map_or(0, String::len);
                    if self.reserve(1, bytes) {
                        self.diagnostics.push(diagnostic);
                    }
                }
                let test = TestResult {
                    label,
                    status,
                    duration_ms: u64::try_from(summary.total_run_duration_millis).ok(),
                    attempts: u32::try_from(summary.attempt_count.max(1)).unwrap_or(1),
                    shard: u32::try_from(summary.shard_count)
                        .ok()
                        .filter(|value| *value > 0),
                    cases: Vec::new(),
                    test_log_available: false,
                    test_log_unavailable_reason: (status != TestStatus::Passed)
                        .then(|| "test_log_not_snapshotted".to_owned()),
                };
                let bytes = test.label.len()
                    + test
                        .test_log_unavailable_reason
                        .as_ref()
                        .map_or(0, String::len);
                if self.reserve(1, bytes) {
                    self.tests.push(test);
                }
            }
            Some(build_event::Payload::TestResult(result)) => {
                for file in &result.test_action_output {
                    if let Some(artifact) = file_artifact(file) {
                        self.push_direct_artifact(artifact);
                    }
                }
            }
            Some(build_event::Payload::NamedSetOfFiles(set)) => {
                if let Some(build_event_id::Id::NamedSet(named_set)) =
                    id.as_ref().and_then(|id| id.id.as_ref())
                {
                    let files = set
                        .files
                        .iter()
                        .filter_map(file_artifact)
                        .collect::<Vec<_>>();
                    let children = set
                        .file_sets
                        .iter()
                        .map(|set| bounded_text(set.id, 4 * 1024))
                        .collect::<Vec<_>>();
                    let key = bounded_text(named_set.id, 4 * 1024);
                    let bytes = key.len()
                        + files
                            .iter()
                            .map(|artifact| artifact.name.len() + artifact.uri.len())
                            .sum::<usize>()
                        + children.iter().map(String::len).sum::<usize>();
                    let items = 1_usize
                        .saturating_add(files.len())
                        .saturating_add(children.len());
                    if self.reserve(items, bytes) {
                        self.named_sets
                            .insert(key, OwnedNamedSet { files, children });
                    }
                }
            }
            Some(build_event::Payload::OptionsParsed(options))
                if self.canonical_arguments.is_none() =>
            {
                let mut arguments = options
                    .startup_options
                    .iter()
                    .chain(options.cmd_line.iter())
                    .map(|value| bounded_text(value, 64 * 1024))
                    .collect::<Vec<_>>();
                let bytes = arguments.iter().map(String::len).sum();
                if self.reserve(arguments.len(), bytes) {
                    self.canonical_arguments = Some(std::mem::take(&mut arguments));
                }
            }
            _ => {}
        }
    }

    #[must_use]
    pub fn finish(
        mut self,
        stdout: &[u8],
        stderr: &[u8],
        exit_code: Option<i32>,
        elapsed_ms: u64,
        budget: Budget,
    ) -> StreamReductionOutput {
        let success = exit_code == Some(0);
        if !success {
            add_text_diagnostics(stderr, &mut self.diagnostics);
            add_text_diagnostics(stdout, &mut self.diagnostics);
        }

        self.targets
            .sort_by(|left, right| left.label.cmp(&right.label));
        self.targets
            .dedup_by(|left, right| left.label == right.label);
        self.tests
            .sort_by(|left, right| left.label.cmp(&right.label));
        self.tests.dedup_by(|left, right| left.label == right.label);

        let target_counts = TargetCounts {
            requested: self.targets.len(),
            succeeded: self.targets.iter().filter(|target| target.success).count(),
            failed: self.targets.iter().filter(|target| !target.success).count(),
        };
        let mut test_counts = TestCounts::default();
        for test in &self.tests {
            match test.status {
                TestStatus::Passed => test_counts.passed += 1,
                TestStatus::Failed => test_counts.failed += 1,
                TestStatus::Flaky => test_counts.flaky += 1,
                TestStatus::Skipped => test_counts.skipped += 1,
                TestStatus::TimedOut | TestStatus::Incomplete | TestStatus::Remote => {
                    test_counts.incomplete += 1;
                }
            }
        }

        let artifacts = self.resolve_artifacts();
        let mut summary = InvocationSummary {
            success,
            headline: if success {
                format!("Bazel completed successfully in {elapsed_ms} ms")
            } else {
                format!("Bazel failed with exit code {exit_code:?}")
            },
            targets: self.targets,
            target_counts,
            diagnostics: self.diagnostics,
            tests: self.tests,
            test_counts,
            coverage: None,
            query_sample: Vec::new(),
            query_result_count: None,
            elapsed_ms,
            truncated: self.truncated,
            inspect_hint: self.truncated.then(|| "diagnostics".to_owned()),
        };
        finalize_diagnostics(&mut summary, budget);
        StreamReductionOutput {
            summary,
            artifacts,
            canonical_arguments: self.canonical_arguments,
        }
    }

    fn reserve(&mut self, items: usize, bytes: usize) -> bool {
        let next_items = self.retained_items.saturating_add(items);
        let next_bytes = self.retained_bytes.saturating_add(bytes);
        if next_items > STREAM_MAX_ITEMS || next_bytes > STREAM_MAX_RETAINED_BYTES {
            self.truncated = true;
            return false;
        }
        self.retained_items = next_items;
        self.retained_bytes = next_bytes;
        true
    }

    fn push_root(&mut self, id: &str) {
        let id = bounded_text(id, 4 * 1024);
        if self.reserve(1, id.len()) {
            self.artifact_roots.push(id);
        }
    }

    fn push_direct_artifact(&mut self, artifact: Artifact) {
        let bytes = artifact.name.len() + artifact.uri.len();
        if self.reserve(1, bytes) {
            self.direct_artifacts.push(artifact);
        }
    }

    fn resolve_artifacts(&mut self) -> Vec<Artifact> {
        let mut visited = BTreeSet::new();
        let mut pending = std::mem::take(&mut self.artifact_roots);
        let mut artifacts = std::mem::take(&mut self.direct_artifacts);
        while let Some(id) = pending.pop() {
            if !visited.insert(id.clone()) {
                continue;
            }
            if let Some(set) = self.named_sets.get(&id) {
                artifacts.extend(set.files.iter().cloned());
                pending.extend(set.children.iter().cloned());
            }
        }
        let mut seen = BTreeSet::new();
        artifacts
            .into_iter()
            .filter(|artifact| seen.insert((artifact.name.clone(), artifact.uri.clone())))
            .collect()
    }
}

#[must_use]
pub fn reduce_invocation(input: ReductionInput<'_>) -> InvocationSummary {
    let success = input.exit_code == Some(0);
    let mut diagnostics = Vec::new();
    let mut targets = Vec::new();
    let mut tests = Vec::new();

    for event in input.events {
        let event = event.view();
        let id = decode_event_id(event.id).ok();
        match event.payload.as_ref() {
            Some(build_event::Payload::Aborted(aborted)) => diagnostics.push(Diagnostic {
                severity: Severity::Error,
                category: abort_category(aborted.reason),
                message: aborted.description.to_owned(),
                location: None,
                target: label_from_id(id.as_ref()),
                action: None,
                repetition_count: 1,
            }),
            Some(build_event::Payload::Action(action)) if !action.success => {
                diagnostics.push(Diagnostic {
                    severity: Severity::Error,
                    category: DiagnosticCategory::Action,
                    message: format!(
                        "{} action failed with exit code {}",
                        if action.r#type.is_empty() {
                            "Bazel".to_owned()
                        } else {
                            action.r#type.to_owned()
                        },
                        action.exit_code
                    ),
                    location: None,
                    target: label_from_id(id.as_ref()).or_else(|| nonempty(action.label)),
                    action: nonempty(action.r#type),
                    repetition_count: 1,
                });
            }
            Some(build_event::Payload::Completed(completed)) => targets.push(TargetResult {
                label: label_from_id(id.as_ref()).unwrap_or_else(|| "<unknown target>".into()),
                success: completed.success,
            }),
            Some(build_event::Payload::TestSummary(summary)) => {
                let label = label_from_id(id.as_ref()).unwrap_or_else(|| "<unknown test>".into());
                let status = test_status(summary.overall_status);
                if let Some(diagnostic) = test_outcome_diagnostic(&label, status) {
                    diagnostics.push(diagnostic);
                }
                tests.push(TestResult {
                    label,
                    status,
                    duration_ms: u64::try_from(summary.total_run_duration_millis).ok(),
                    attempts: u32::try_from(summary.attempt_count.max(1)).unwrap_or(1),
                    shard: u32::try_from(summary.shard_count)
                        .ok()
                        .filter(|value| *value > 0),
                    cases: Vec::new(),
                    test_log_available: false,
                    test_log_unavailable_reason: (status != TestStatus::Passed)
                        .then(|| "test_log_not_snapshotted".to_owned()),
                });
            }
            _ => {}
        }
    }

    if !success {
        add_text_diagnostics(input.stderr, &mut diagnostics);
        add_text_diagnostics(input.stdout, &mut diagnostics);
    }

    targets.sort_by(|left, right| left.label.cmp(&right.label));
    targets.dedup_by(|left, right| left.label == right.label);
    tests.sort_by(|left, right| left.label.cmp(&right.label));
    tests.dedup_by(|left, right| left.label == right.label);
    let target_counts = TargetCounts {
        requested: targets.len(),
        succeeded: targets.iter().filter(|target| target.success).count(),
        failed: targets.iter().filter(|target| !target.success).count(),
    };
    let mut test_counts = TestCounts::default();
    for test in &tests {
        match test.status {
            TestStatus::Passed => test_counts.passed += 1,
            TestStatus::Failed => test_counts.failed += 1,
            TestStatus::Flaky => test_counts.flaky += 1,
            TestStatus::Skipped => test_counts.skipped += 1,
            TestStatus::TimedOut | TestStatus::Incomplete | TestStatus::Remote => {
                test_counts.incomplete += 1;
            }
        }
    }
    let mut summary = InvocationSummary {
        success,
        headline: if success {
            format!("Bazel completed successfully in {} ms", input.elapsed_ms)
        } else {
            format!("Bazel failed with exit code {:?}", input.exit_code)
        },
        targets,
        target_counts,
        diagnostics,
        tests,
        test_counts,
        coverage: None,
        query_sample: Vec::new(),
        query_result_count: None,
        elapsed_ms: input.elapsed_ms,
        truncated: false,
        inspect_hint: None,
    };
    finalize_diagnostics(&mut summary, input.budget);
    summary
}

fn deduplicate_diagnostics(diagnostics: Vec<Diagnostic>) -> Vec<Diagnostic> {
    let mut positions = BTreeMap::<
        (
            Severity,
            DiagnosticCategory,
            String,
            Option<String>,
            Option<String>,
        ),
        usize,
    >::new();
    let mut unique = Vec::<Diagnostic>::new();
    for diagnostic in diagnostics {
        let aggregate_actions = diagnostic.category == DiagnosticCategory::Action;
        let key = (
            diagnostic.severity,
            diagnostic.category,
            diagnostic.message.clone(),
            (!aggregate_actions)
                .then(|| diagnostic.target.clone())
                .flatten(),
            diagnostic.action.clone(),
        );
        if let Some(index) = positions.get(&key).copied() {
            if aggregate_actions && unique[index].target != diagnostic.target {
                unique[index].target = None;
            }
            unique[index].repetition_count = unique[index]
                .repetition_count
                .saturating_add(diagnostic.repetition_count);
        } else {
            positions.insert(key, unique.len());
            unique.push(diagnostic);
        }
    }
    unique
}

/// Re-ranks, aggregates, and bounds diagnostics after all structured and local
/// evidence enrichment has completed.
pub fn finalize_diagnostics(summary: &mut InvocationSummary, budget: Budget) {
    let diagnostics = std::mem::take(&mut summary.diagnostics);
    let mut diagnostics = deduplicate_diagnostics(diagnostics);
    diagnostics.sort_by_key(diagnostic_priority);

    let mut truncated = diagnostics.len() > budget.max_items;
    diagnostics.truncate(budget.max_items);
    let mut used = 0_usize;
    diagnostics.retain(|diagnostic| {
        let next = used.saturating_add(diagnostic.message.len());
        if next > budget.max_bytes {
            truncated = true;
            false
        } else {
            used = next;
            true
        }
    });
    summary.diagnostics = diagnostics;
    summary.truncated |= truncated;
    if summary.truncated {
        summary.inspect_hint = Some("diagnostics".to_owned());
    }
    if !summary.success
        && let Some(first) = summary.diagnostics.first()
    {
        summary.headline = format!("Bazel failed: {}", first.message);
    }
}

fn diagnostic_priority(diagnostic: &Diagnostic) -> (Severity, u8, u8) {
    let category = match diagnostic.category {
        DiagnosticCategory::Loading
        | DiagnosticCategory::Visibility
        | DiagnosticCategory::Analysis
        | DiagnosticCategory::Compilation => 0,
        DiagnosticCategory::Test => 1,
        DiagnosticCategory::Workspace => 2,
        DiagnosticCategory::Bazel => 3,
        DiagnosticCategory::Unknown => 4,
        DiagnosticCategory::Action => 5,
    };
    let lower = diagnostic.message.to_ascii_lowercase();
    let evidence_quality = if lower.starts_with("test failed:")
        || lower.starts_with("test timed out:")
        || lower.starts_with("test was incomplete:")
        || lower.starts_with("test result was unavailable:")
    {
        3
    } else if lower.contains("root_cause") && !lower.contains("error executing") {
        0
    } else if lower.contains("error executing") {
        2
    } else {
        1
    };
    (diagnostic.severity, category, evidence_quality)
}

fn test_outcome_diagnostic(label: &str, status: TestStatus) -> Option<Diagnostic> {
    let (severity, message) = match status {
        TestStatus::Passed | TestStatus::Skipped => return None,
        TestStatus::Flaky => (Severity::Warning, format!("Test was flaky: {label}")),
        TestStatus::Failed => (Severity::Error, format!("Test failed: {label}")),
        TestStatus::TimedOut => (Severity::Error, format!("Test timed out: {label}")),
        TestStatus::Incomplete => (Severity::Error, format!("Test was incomplete: {label}")),
        TestStatus::Remote => (
            Severity::Error,
            format!("Test result was unavailable: {label}"),
        ),
    };
    Some(Diagnostic {
        severity,
        category: DiagnosticCategory::Test,
        message,
        location: None,
        target: Some(label.to_owned()),
        action: None,
        repetition_count: 1,
    })
}

#[must_use]
pub fn reduce_artifacts<'a>(events: &'a [BepEvent]) -> Vec<Artifact> {
    let mut sets = BTreeMap::<&'a str, &'a NamedSetOfFilesView<'a>>::new();
    let mut roots = Vec::<&'a str>::new();
    let mut direct = Vec::<&'a FileView<'a>>::new();
    for event in events {
        let event = event.view();
        let id = decode_event_id(event.id).ok();
        match event.payload.as_ref() {
            Some(build_event::Payload::NamedSetOfFiles(set)) => {
                if let Some(build_event_id::Id::NamedSet(named_set)) =
                    id.as_ref().and_then(|id| id.id.as_ref())
                {
                    sets.insert(named_set.id, set);
                }
            }
            Some(build_event::Payload::Completed(completed)) => {
                for group in &completed.output_group {
                    roots.extend(group.file_sets.iter().map(|set| set.id));
                    direct.extend(group.inline_files.iter());
                }
                direct.extend(completed.important_output.iter());
                direct.extend(completed.directory_output.iter());
            }
            Some(build_event::Payload::Action(action)) if !action.success => {
                if let Some(output) = action.primary_output.as_option() {
                    direct.push(output);
                }
            }
            Some(build_event::Payload::TestResult(result)) => {
                direct.extend(result.test_action_output.iter());
            }
            _ => {}
        }
    }
    let mut visited = BTreeSet::<&str>::new();
    let mut pending = roots;
    while let Some(id) = pending.pop() {
        if !visited.insert(id) {
            continue;
        }
        if let Some(set) = sets.get(id) {
            direct.extend(set.files.iter());
            pending.extend(set.file_sets.iter().map(|set| set.id));
        }
    }
    let mut seen = BTreeSet::new();
    direct
        .iter()
        .filter_map(|file| file_artifact(file))
        .filter(|artifact| seen.insert((artifact.name.clone(), artifact.uri.clone())))
        .collect()
}

#[must_use]
pub fn extract_canonical_arguments(events: &[BepEvent]) -> Option<Vec<String>> {
    events
        .iter()
        .find_map(|event| match event.view().payload.as_ref() {
            Some(build_event::Payload::OptionsParsed(options)) => {
                let mut arguments = options
                    .startup_options
                    .iter()
                    .map(|value| (*value).to_owned())
                    .collect::<Vec<_>>();
                arguments.extend(options.cmd_line.iter().map(|value| (*value).to_owned()));
                Some(arguments)
            }
            _ => None,
        })
}

fn file_artifact(file: &FileView<'_>) -> Option<Artifact> {
    let name = bounded_text(
        &file
            .path_prefix
            .iter()
            .chain(std::iter::once(&file.name))
            .filter(|part| !part.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join("/"),
        1_000,
    );
    let (uri, kind, locally_available) = match &file.file {
        Some(file::File::Uri(uri)) => {
            let kind = if file.name == "test.log" || file.name == "test.xml" {
                ArtifactKind::TestLog
            } else if file.name.contains("coverage") || file.name.ends_with(".dat") {
                ArtifactKind::Coverage
            } else if uri.starts_with("file:") {
                ArtifactKind::File
            } else {
                ArtifactKind::Remote
            };
            (bounded_text(uri, 1_000), kind, uri.starts_with("file:"))
        }
        Some(file::File::SymlinkTargetPath(target)) => {
            (bounded_text(target, 1_000), ArtifactKind::File, true)
        }
        Some(file::File::Contents(_)) => ("inline://redacted".to_owned(), ArtifactKind::File, true),
        None => return None,
    };
    Some(Artifact {
        name,
        kind,
        uri,
        size_bytes: u64::try_from(file.length).ok(),
        locally_available,
    })
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

fn add_text_diagnostics(input: &[u8], diagnostics: &mut Vec<Diagnostic>) {
    let normalized = normalize_terminal_text(input);
    let candidates = deduplicate_lines(&normalized);
    for (line, count) in candidates
        .into_iter()
        .filter(|(line, _)| is_actionable(line))
    {
        diagnostics.push(Diagnostic {
            severity: if line.to_ascii_lowercase().contains("warning:") {
                Severity::Warning
            } else {
                Severity::Error
            },
            category: category_from_text(&line),
            message: line,
            location: None,
            target: None,
            action: None,
            repetition_count: count,
        });
    }
}

fn is_actionable(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("error:")
        || lower.starts_with("error ")
        || lower.contains("failed:")
        || lower.contains("no such target")
        || lower.contains("no such package")
        || lower.contains("visibility error")
        || lower.contains("undefined reference")
        || lower.contains("fatal:")
        || lower.contains("root_cause")
}

fn category_from_text(line: &str) -> DiagnosticCategory {
    let lower = line.to_ascii_lowercase();
    if lower.contains("no such package") || lower.contains("no such target") {
        DiagnosticCategory::Loading
    } else if lower.contains("visibility") {
        DiagnosticCategory::Visibility
    } else if lower.contains("analysis") {
        DiagnosticCategory::Analysis
    } else if lower.contains("test") {
        DiagnosticCategory::Test
    } else if lower.contains("error:")
        || lower.contains("error[")
        || lower.contains("undefined reference")
    {
        DiagnosticCategory::Compilation
    } else if lower.contains("root_cause") {
        DiagnosticCategory::Test
    } else {
        DiagnosticCategory::Unknown
    }
}

fn abort_category(reason: i32) -> DiagnosticCategory {
    match reason {
        5 => DiagnosticCategory::Loading,
        6 => DiagnosticCategory::Analysis,
        _ => DiagnosticCategory::Bazel,
    }
}

fn label_from_id(id: Option<&BuildEventIdView<'_>>) -> Option<String> {
    match id.and_then(|value| value.id.as_ref()) {
        Some(build_event_id::Id::TargetCompleted(value)) => nonempty(value.label),
        Some(build_event_id::Id::ActionCompleted(value)) => nonempty(value.label),
        Some(build_event_id::Id::TestSummary(value)) => nonempty(value.label),
        Some(build_event_id::Id::TestResult(value)) => nonempty(value.label),
        Some(build_event_id::Id::UnconfiguredLabel(value)) => nonempty(value.label),
        Some(build_event_id::Id::ConfiguredLabel(value)) => nonempty(value.label),
        _ => None,
    }
}

fn nonempty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}

fn test_status(value: i32) -> TestStatus {
    match value {
        1 => TestStatus::Passed,
        2 => TestStatus::Flaky,
        3 => TestStatus::TimedOut,
        4 | 7 | 8 => TestStatus::Failed,
        5 => TestStatus::Incomplete,
        6 => TestStatus::Remote,
        _ => TestStatus::Incomplete,
    }
}

#[cfg(test)]
mod tests {
    use bazel_mcp_bep::proto::{
        ActionExecuted, BuildEvent, BuildEventId, File, FileOwnedView, NamedSetOfFiles,
    };
    use bazel_mcp_bep::proto::{
        build_event as owned_build_event, build_event_id as owned_build_event_id,
        file as owned_file,
    };
    use bazel_mcp_bep::{BepEvent, encode_event_id};

    use super::*;

    #[test]
    fn reduces_noisy_failure_to_root_cause() {
        let event = BuildEvent {
            payload: Some(owned_build_event::Payload::Action(Box::new(
                ActionExecuted {
                    success: false,
                    exit_code: 1,
                    r#type: "CppCompile".into(),
                    ..Default::default()
                },
            ))),
            ..Default::default()
        };
        let event = BepEvent::from_owned(&event).unwrap();
        let stderr = b"warning: duplicate\nwarning: duplicate\nfile.cc:7: error: bad type\n";
        let summary = reduce_invocation(ReductionInput {
            events: &[event],
            stdout: b"",
            stderr,
            exit_code: Some(1),
            elapsed_ms: 12,
            budget: Budget::result_default(),
        });
        assert!(!summary.success);
        assert!(
            summary
                .diagnostics
                .iter()
                .any(|d| d.message.contains("bad type"))
        );
        assert!(summary.diagnostics.len() <= 2);
    }

    #[test]
    fn ranks_root_cause_before_aggregated_fanout_failures() {
        let events = (0..48)
            .map(|index| {
                let event = BuildEvent {
                    id: encode_event_id(&BuildEventId {
                        id: Some(owned_build_event_id::Id::ActionCompleted(Box::new(
                            bazel_mcp_bep::proto::build_event_id::ActionCompletedId {
                                label: format!("//pkg:fanout_{index}"),
                                ..Default::default()
                            },
                        ))),
                    }),
                    payload: Some(owned_build_event::Payload::Action(Box::new(
                        ActionExecuted {
                            success: false,
                            exit_code: 1,
                            r#type: "CppCompile".into(),
                            ..Default::default()
                        },
                    ))),
                    ..Default::default()
                };
                BepEvent::from_owned(&event).unwrap()
            })
            .collect::<Vec<_>>();
        let summary = reduce_invocation(ReductionInput {
            events: &events,
            stdout: b"",
            stderr: b"source.cc:9: error: FANOUT_ROOT_CAUSE",
            exit_code: Some(1),
            elapsed_ms: 1,
            budget: Budget {
                max_bytes: 1_000,
                max_items: 2,
            },
        });

        assert!(summary.diagnostics[0].message.contains("FANOUT_ROOT_CAUSE"));
        let action = summary
            .diagnostics
            .iter()
            .find(|diagnostic| diagnostic.category == DiagnosticCategory::Action)
            .unwrap();
        assert_eq!(action.target, None);
        assert_eq!(action.repetition_count, 48);
    }

    #[test]
    fn recognizes_cross_language_and_loading_root_causes() {
        for (line, category) in [
            (
                "ERROR: no such target '//missing:one'",
                DiagnosticCategory::Loading,
            ),
            (
                "ERROR: target is not visible (visibility error)",
                DiagnosticCategory::Visibility,
            ),
            (
                "Main.java:7: error: cannot find symbol JAVA_ROOT_CAUSE",
                DiagnosticCategory::Compilation,
            ),
            (
                "src/lib.rs:4: error[E0425]: RUST_ROOT_CAUSE",
                DiagnosticCategory::Compilation,
            ),
            (
                "custom tool ERROR: CUSTOM_ROOT_CAUSE",
                DiagnosticCategory::Compilation,
            ),
        ] {
            let summary = reduce_invocation(ReductionInput {
                events: &[],
                stdout: b"",
                stderr: line.as_bytes(),
                exit_code: Some(1),
                elapsed_ms: 1,
                budget: Budget::result_default(),
            });
            assert!(summary.diagnostics.iter().any(|diagnostic| {
                diagnostic.category == category
                    && (diagnostic.message.contains("ROOT_CAUSE")
                        || diagnostic.message.contains("no such target")
                        || diagnostic.message.contains("visibility"))
            }));
        }
    }

    #[test]
    fn resolves_nested_named_sets_once_even_when_cyclic() {
        fn event_id(id: &str) -> Vec<u8> {
            encode_event_id(&BuildEventId {
                id: Some(owned_build_event_id::Id::NamedSet(Box::new(
                    bazel_mcp_bep::proto::build_event_id::NamedSetOfFilesId { id: id.into() },
                ))),
            })
        }
        let first = BuildEvent {
            id: event_id("first"),
            payload: Some(owned_build_event::Payload::NamedSetOfFiles(Box::new(
                NamedSetOfFiles {
                    files: vec![File {
                        name: "local.out".into(),
                        file: Some(owned_file::File::Uri("file:///tmp/local.out".into())),
                        ..Default::default()
                    }],
                    file_sets: vec![bazel_mcp_bep::proto::build_event_id::NamedSetOfFilesId {
                        id: "second".into(),
                    }],
                },
            ))),
            ..Default::default()
        };
        let second = BuildEvent {
            id: event_id("second"),
            payload: Some(owned_build_event::Payload::NamedSetOfFiles(Box::new(
                NamedSetOfFiles {
                    files: vec![File {
                        name: "remote.out".into(),
                        file: Some(owned_file::File::Uri("bytestream://cache/digest".into())),
                        ..Default::default()
                    }],
                    file_sets: vec![bazel_mcp_bep::proto::build_event_id::NamedSetOfFilesId {
                        id: "first".into(),
                    }],
                },
            ))),
            ..Default::default()
        };
        let completed = BuildEvent {
            payload: Some(owned_build_event::Payload::Completed(Box::new(
                bazel_mcp_bep::proto::TargetComplete {
                    output_group: vec![bazel_mcp_bep::proto::OutputGroup {
                        file_sets: vec![bazel_mcp_bep::proto::build_event_id::NamedSetOfFilesId {
                            id: "first".into(),
                        }],
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            ))),
            ..Default::default()
        };
        let completed = BepEvent::from_owned(&completed).unwrap();
        let second = BepEvent::from_owned(&second).unwrap();
        let first = BepEvent::from_owned(&first).unwrap();
        let events = [completed, second, first];
        let artifacts = reduce_artifacts(&events);
        assert_eq!(artifacts.len(), 2);
        assert!(artifacts.iter().any(|artifact| {
            artifact.kind == ArtifactKind::Remote && !artifact.locally_available
        }));

        let mut streaming = BepAccumulator::default();
        for event in events {
            streaming.observe(event);
        }
        let output = streaming.finish(b"", b"", Some(0), 1, Budget::result_default());
        assert_eq!(output.artifacts, artifacts);
    }

    #[test]
    fn maps_bazel_test_outcomes_without_losing_failure_classes() {
        assert_eq!(test_status(1), TestStatus::Passed);
        assert_eq!(test_status(2), TestStatus::Flaky);
        assert_eq!(test_status(3), TestStatus::TimedOut);
        assert_eq!(test_status(4), TestStatus::Failed);
        assert_eq!(test_status(5), TestStatus::Incomplete);
        assert_eq!(test_status(6), TestStatus::Remote);
    }

    #[test]
    fn bounds_symlink_artifact_paths() {
        let file = File {
            name: "artifact".into(),
            file: Some(owned_file::File::SymlinkTargetPath("x".repeat(2_000))),
            ..Default::default()
        };
        let file = FileOwnedView::from_owned(&file).unwrap();
        let artifact = file_artifact(file.view()).unwrap();
        assert!(artifact.uri.len() <= 1_003);
        assert!(artifact.uri.ends_with('…'));
    }

    #[test]
    fn deduplicates_non_adjacent_diagnostics_without_reordering_root_causes() {
        let diagnostic = |message: &str| Diagnostic {
            severity: Severity::Error,
            category: DiagnosticCategory::Compilation,
            message: message.into(),
            location: None,
            target: Some("//pkg:target".into()),
            action: None,
            repetition_count: 1,
        };
        let deduplicated = deduplicate_diagnostics(vec![
            diagnostic("first"),
            diagnostic("second"),
            diagnostic("first"),
        ]);
        assert_eq!(
            deduplicated
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "second"]
        );
        assert_eq!(deduplicated[0].repetition_count, 2);
    }
}
