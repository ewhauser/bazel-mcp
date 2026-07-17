use bazel_mcp_bep::{
    BepEvent, BorrowedBepEvent, decode_event_id,
    view::{BuildEventIdView, FileView, NamedSetOfFilesView, build_event, build_event_id, file},
};
use bazel_mcp_types::{
    Artifact, ArtifactKind, Diagnostic, DiagnosticCategory, DiagnosticLocation, InvocationSummary,
    Severity, TargetCounts, TargetResult, TestCounts, TestResult, TestStatus,
};
use std::collections::{BTreeMap, BTreeSet};

use crate::{Budget, ReducerEvent, ReducerEventKind, deduplicate_lines, normalize_terminal_text};

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
    extension: Option<ExtensionEventCollector>,
    next_event_ordinal: u64,
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
    pub reducer_events: Vec<ReducerEvent>,
    pub reducer_input_truncated: bool,
}

struct ExtensionEventCollector {
    events: Vec<ReducerEvent>,
    retained_bytes: usize,
    max_events: usize,
    max_bytes: usize,
    truncated: bool,
}

impl BepAccumulator {
    #[must_use]
    pub fn with_extension_events(max_events: usize, max_bytes: usize) -> Self {
        Self {
            extension: Some(ExtensionEventCollector {
                events: Vec::new(),
                retained_bytes: 0,
                max_events,
                max_bytes,
                truncated: false,
            }),
            ..Self::default()
        }
    }

    pub fn observe(&mut self, event: BepEvent) {
        self.observe_view(event.view());
    }

    pub fn observe_borrowed(&mut self, event: BorrowedBepEvent<'_>) {
        self.observe_view(event.view());
    }

    fn observe_view(&mut self, event: &bazel_mcp_bep::view::BuildEventView<'_>) {
        let id = decode_event_id(event.id).ok();
        let ordinal = self.next_event_ordinal;
        self.next_event_ordinal = self.next_event_ordinal.saturating_add(1);
        self.observe_extension_event(event, id.as_ref(), ordinal);
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
        let (reducer_events, reducer_input_truncated) = self.extension.take().map_or_else(
            || (Vec::new(), false),
            |collector| (collector.events, collector.truncated),
        );
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
            reducer_events,
            reducer_input_truncated,
        }
    }

    fn observe_extension_event(
        &mut self,
        event: &bazel_mcp_bep::view::BuildEventView<'_>,
        id: Option<&BuildEventIdView<'_>>,
        ordinal: u64,
    ) {
        let Some(collector) = &mut self.extension else {
            return;
        };
        let event = match event.payload.as_ref() {
            Some(build_event::Payload::Aborted(aborted)) => Some(ReducerEvent {
                ordinal,
                kind: ReducerEventKind::Aborted,
                label: label_from_id(id),
                target_kind: None,
                action_type: None,
                success: Some(false),
                exit_code: None,
                message: Some(bounded_text(aborted.description, 64 * 1024)),
            }),
            Some(build_event::Payload::Action(action)) => Some(ReducerEvent {
                ordinal,
                kind: ReducerEventKind::Action,
                label: label_from_id(id).or_else(|| nonempty(action.label)),
                target_kind: None,
                action_type: nonempty(action.r#type),
                success: Some(action.success),
                exit_code: Some(action.exit_code),
                message: None,
            }),
            Some(build_event::Payload::Completed(completed)) => Some(ReducerEvent {
                ordinal,
                kind: ReducerEventKind::Target,
                label: label_from_id(id),
                target_kind: nonempty(completed.target_kind),
                action_type: None,
                success: Some(completed.success),
                exit_code: None,
                message: None,
            }),
            Some(build_event::Payload::TestSummary(summary)) => Some(ReducerEvent {
                ordinal,
                kind: ReducerEventKind::TestSummary,
                label: label_from_id(id),
                target_kind: None,
                action_type: None,
                success: Some(matches!(
                    test_status(summary.overall_status),
                    TestStatus::Passed
                )),
                exit_code: None,
                message: None,
            }),
            _ => None,
        };
        if let Some(event) = event {
            collector.push(event);
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

impl ExtensionEventCollector {
    fn push(&mut self, event: ReducerEvent) {
        let bytes = event.label.as_ref().map_or(0, String::len)
            + event.target_kind.as_ref().map_or(0, String::len)
            + event.action_type.as_ref().map_or(0, String::len)
            + event.message.as_ref().map_or(0, String::len);
        if self.events.len() >= self.max_events
            || self.retained_bytes.saturating_add(bytes) > self.max_bytes
        {
            self.truncated = true;
            return;
        }
        self.retained_bytes = self.retained_bytes.saturating_add(bytes);
        self.events.push(event);
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
            Option<(String, Option<u32>, Option<u32>)>,
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
            diagnostic
                .location
                .as_ref()
                .map(|location| (location.path.clone(), location.line, location.column)),
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
    let message = diagnostic.message.as_str();
    let rust_failure = contains_ignore_ascii_case(message, "panicked at")
        || (contains_ignore_ascii_case(message, "assertion")
            && contains_ignore_ascii_case(message, " failed"));
    let evidence_quality = if diagnostic.location.is_some()
        || (contains_ignore_ascii_case(message, "root_cause")
            && !contains_ignore_ascii_case(message, "error executing"))
    {
        0
    } else if diagnostic.category == DiagnosticCategory::Test && rust_failure {
        1
    } else if starts_with_ignore_ascii_case(message, "test failed:")
        || starts_with_ignore_ascii_case(message, "test timed out:")
        || starts_with_ignore_ascii_case(message, "test was incomplete:")
        || starts_with_ignore_ascii_case(message, "test result was unavailable:")
    {
        3
    } else if contains_ignore_ascii_case(message, "error executing") {
        2
    } else {
        1
    };
    (diagnostic.severity, category, evidence_quality)
}

fn contains_ignore_ascii_case(value: &str, needle: &str) -> bool {
    let needle = needle.as_bytes();
    needle.is_empty()
        || value
            .as_bytes()
            .windows(needle.len())
            .any(|window| window.eq_ignore_ascii_case(needle))
}

fn starts_with_ignore_ascii_case(value: &str, prefix: &str) -> bool {
    value
        .as_bytes()
        .get(..prefix.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(prefix.as_bytes()))
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
    let swc = parse_swc_diagnostics(&normalized);
    let has_swc_diagnostics = !swc.diagnostics.is_empty();
    diagnostics.extend(swc.diagnostics);
    let mut cpp_linker_parser = CppLinkerDiagnosticParser::default();
    for line in normalized.lines() {
        if let Some(diagnostic) = cpp_linker_parser.observe_line(line) {
            diagnostics.push(diagnostic);
        }
    }
    diagnostics.extend(parse_java_compiler_diagnostics(&normalized));
    let mut javascript_parser = JavaScriptTestDiagnosticParser::default();
    let mut javascript_test_messages = BTreeSet::new();
    for line in normalized.lines() {
        if let Some(diagnostic) = javascript_parser.observe_line(line) {
            javascript_test_messages.insert(diagnostic.message.clone());
            diagnostics.push(diagnostic);
        }
    }
    if let Some(diagnostic) = javascript_parser.finish() {
        javascript_test_messages.insert(diagnostic.message.clone());
        diagnostics.push(diagnostic);
    }
    let mut java_parser = JavaTestDiagnosticParser::default();
    let mut java_test_messages = BTreeSet::new();
    for line in normalized.lines() {
        if let Some(diagnostic) = java_parser.observe_line(line) {
            java_test_messages.insert(diagnostic.message.clone());
            diagnostics.push(diagnostic);
        }
    }
    if let Some(diagnostic) = java_parser.finish() {
        java_test_messages.insert(diagnostic.message.clone());
        diagnostics.push(diagnostic);
    }
    let mut starlark_parser = StarlarkDiagnosticParser::default();
    for line in normalized.lines() {
        if let Some(diagnostic) = starlark_parser.observe_line(line) {
            diagnostics.push(diagnostic);
        }
    }
    let mut python_parser = PythonDiagnosticParser::default();
    for line in normalized.lines() {
        if javascript_exception_message(line)
            .is_some_and(|message| javascript_test_messages.contains(message))
            || java_exception_message(line)
                .is_some_and(|message| java_test_messages.contains(message))
        {
            continue;
        }
        if let Some(diagnostic) = python_parser.observe_line(line) {
            diagnostics.push(diagnostic);
        }
    }
    let candidates = deduplicate_lines(&normalized);
    let has_strict_dependency_block = candidates.iter().any(|(line, _)| {
        line.to_ascii_lowercase()
            .contains("missing strict dependencies")
    });
    let strict_dependency_count = candidates
        .iter()
        .filter(|(line, _)| strict_dependency_diagnostic(line).is_some())
        .count();

    for (line, count) in candidates {
        if swc.consumed_lines.contains(line.trim())
            || (has_swc_diagnostics && is_swc_action_wrapper(&line))
        {
            continue;
        }
        if has_strict_dependency_block
            && let Some(mut diagnostic) = strict_dependency_diagnostic(&line)
        {
            diagnostic.repetition_count = count;
            diagnostics.push(diagnostic);
            continue;
        }
        if let Some(mut diagnostic) = parse_cpp_diagnostic(&line) {
            diagnostic.repetition_count = count;
            diagnostics.push(diagnostic);
            continue;
        }
        if let Some(mut diagnostic) = parse_typescript_diagnostic(&line) {
            diagnostic.repetition_count = count;
            diagnostics.push(diagnostic);
            continue;
        }
        if let Some(mut diagnostic) = parse_protobuf_diagnostic(&line) {
            diagnostic.repetition_count = count;
            diagnostics.push(diagnostic);
            continue;
        }
        if let Some(mut diagnostic) = parse_go_diagnostic(&line) {
            diagnostic.repetition_count = count;
            diagnostics.push(diagnostic);
            continue;
        }
        if parse_cpp_linker_diagnostic(&line).is_some() {
            continue;
        }
        if parse_java_compiler_diagnostic(&line).is_some()
            || javascript_exception_message(&line)
                .is_some_and(|message| javascript_test_messages.contains(message))
            || java_exception_message(&line)
                .is_some_and(|message| java_test_messages.contains(message))
        {
            continue;
        }
        if parse_starlark_inline_diagnostic(&line)
            .is_some_and(|diagnostic| is_starlark_root_cause_message(&diagnostic.message))
            || starlark_error_message(&line).is_some()
            || is_starlark_traceback_header(&line)
        {
            continue;
        }
        if parse_python_location(&line).is_some() || python_exception_message(&line).is_some() {
            continue;
        }
        if has_strict_dependency_block
            && strict_dependency_count == 0
            && line
                .to_ascii_lowercase()
                .contains("missing strict dependencies")
        {
            diagnostics.push(Diagnostic {
                severity: Severity::Error,
                category: DiagnosticCategory::Compilation,
                message: line,
                location: None,
                target: None,
                action: None,
                repetition_count: count,
            });
            continue;
        }
        if !is_actionable(&line) {
            continue;
        }
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

fn parse_cpp_diagnostic(line: &str) -> Option<Diagnostic> {
    let line = line
        .trim()
        .strip_prefix("ERROR: ")
        .unwrap_or_else(|| line.trim());
    let (path, line_number, column, message) =
        parse_cpp_colon_location(line).or_else(|| parse_cpp_parenthesized_location(line))?;
    if path.contains(": ") {
        return None;
    }
    let (severity, message) = parse_cpp_severity_message(message)?;
    Some(Diagnostic {
        severity,
        category: DiagnosticCategory::Compilation,
        message: message.to_owned(),
        location: Some(DiagnosticLocation {
            path: compact_cpp_path(path),
            line: Some(line_number),
            column,
        }),
        target: None,
        action: None,
        repetition_count: 1,
    })
}

fn parse_cpp_colon_location(line: &str) -> Option<(&str, u32, Option<u32>, &str)> {
    let path_end = cpp_path_end(line, ':')?;
    let (line_number, remainder) = split_u32_prefix(&line[path_end + 1..])?;
    let (column, message) = split_u32_prefix(remainder)
        .map_or((None, remainder), |(column, message)| {
            (Some(column), message)
        });
    Some((&line[..path_end], line_number, column, message))
}

fn parse_cpp_parenthesized_location(line: &str) -> Option<(&str, u32, Option<u32>, &str)> {
    let path_end = cpp_path_end(line, '(')?;
    let remainder = line[path_end..].strip_prefix('(')?;
    let (coordinates, message) = remainder
        .split_once("): ")
        .or_else(|| remainder.split_once("):"))?;
    let (line_number, column) = coordinates.split_once(',').map_or_else(
        || (coordinates.trim().parse::<u32>().ok(), None),
        |(line_number, column)| {
            (
                line_number.trim().parse::<u32>().ok(),
                column.trim().parse::<u32>().ok(),
            )
        },
    );
    Some((&line[..path_end], line_number?, column, message))
}

fn parse_cpp_severity_message(message: &str) -> Option<(Severity, &str)> {
    let message = message.trim();
    for (marker, severity) in [
        ("fatal error", Severity::Error),
        ("error", Severity::Error),
        ("warning", Severity::Warning),
        ("note", Severity::Note),
    ] {
        let Some(remainder) = message.strip_prefix(marker) else {
            continue;
        };
        let remainder = remainder
            .strip_prefix(':')
            .or_else(|| remainder.strip_prefix(' '))?
            .trim();
        if !remainder.is_empty() {
            return Some((severity, remainder));
        }
    }
    None
}

fn cpp_path_end(line: &str, delimiter: char) -> Option<usize> {
    const EXTENSIONS: [&str; 19] = [
        ".cpp", ".cxx", ".c++", ".cc", ".hpp", ".hxx", ".h++", ".hh", ".ipp", ".tpp", ".inc",
        ".cuh", ".cu", ".mm", ".m", ".C", ".H", ".c", ".h",
    ];
    EXTENSIONS
        .iter()
        .filter_map(|extension| {
            line.rmatch_indices(extension).find_map(|(index, _)| {
                let path_end = index + extension.len();
                line[path_end..].starts_with(delimiter).then_some(path_end)
            })
        })
        .max()
}

#[derive(Debug, Default)]
struct CppLinkerDiagnosticParser {
    apple_undefined_symbols: bool,
    symbols_seen: usize,
}

impl CppLinkerDiagnosticParser {
    const MAX_SYMBOLS: usize = 64;

    fn observe_line(&mut self, line: &str) -> Option<Diagnostic> {
        let line = line.trim();
        if line.starts_with("Undefined symbols for architecture ") {
            self.apple_undefined_symbols = true;
            return None;
        }
        if self.apple_undefined_symbols {
            if let Some(symbol) = parse_apple_undefined_symbol(line) {
                if self.symbols_seen >= Self::MAX_SYMBOLS {
                    return None;
                }
                self.symbols_seen = self.symbols_seen.saturating_add(1);
                return Some(cpp_linker_diagnostic(
                    format!("undefined symbol: {}", bounded_text(symbol, 1_000)),
                    None,
                ));
            }
            if line.starts_with("ld:") || line.starts_with("clang:") {
                self.apple_undefined_symbols = false;
            }
        }
        parse_cpp_linker_diagnostic(line)
    }
}

fn parse_apple_undefined_symbol(line: &str) -> Option<&str> {
    let symbol = line.strip_prefix('"')?;
    let (symbol, _) = symbol.split_once("\", referenced from:")?;
    (!symbol.is_empty()).then_some(symbol)
}

fn parse_cpp_linker_diagnostic(line: &str) -> Option<Diagnostic> {
    let line = line.trim();
    if let Some(index) = line.find("undefined reference to ") {
        let symbol = trim_linker_symbol(&line[index + "undefined reference to ".len()..]);
        if symbol.is_empty() {
            return None;
        }
        return Some(cpp_linker_diagnostic(
            format!("undefined reference to {symbol}"),
            parse_cpp_linker_location(&line[..index]),
        ));
    }
    if let Some(index) = line.find("undefined symbol:") {
        let symbol = trim_linker_symbol(&line[index + "undefined symbol:".len()..]);
        if symbol.is_empty() {
            return None;
        }
        return Some(cpp_linker_diagnostic(
            format!("undefined symbol: {symbol}"),
            None,
        ));
    }
    if let Some(index) = line.find("unresolved external symbol ") {
        let remainder = &line[index + "unresolved external symbol ".len()..];
        let symbol = remainder
            .split_once(" referenced in ")
            .map_or(remainder, |(symbol, _)| symbol)
            .trim();
        if symbol.is_empty() {
            return None;
        }
        return Some(cpp_linker_diagnostic(
            format!("unresolved external symbol {symbol}"),
            None,
        ));
    }
    None
}

fn parse_cpp_linker_location(prefix: &str) -> Option<DiagnosticLocation> {
    let path_end = cpp_path_end(prefix, ':')?;
    let (line_number, _) = split_u32_prefix(&prefix[path_end + 1..])?;
    let path = prefix[..path_end]
        .rsplit_once(": ")
        .map_or(&prefix[..path_end], |(_, path)| path);
    Some(DiagnosticLocation {
        path: compact_cpp_path(path),
        line: Some(line_number),
        column: None,
    })
}

fn trim_linker_symbol(symbol: &str) -> &str {
    symbol
        .trim()
        .trim_end_matches(':')
        .trim_matches(|character| matches!(character, '`' | '\'' | '"'))
}

fn cpp_linker_diagnostic(message: String, location: Option<DiagnosticLocation>) -> Diagnostic {
    Diagnostic {
        severity: Severity::Error,
        category: DiagnosticCategory::Compilation,
        message,
        location,
        target: None,
        action: None,
        repetition_count: 1,
    }
}

fn compact_cpp_path(path: &str) -> String {
    let path = strip_workspace_marker(path.trim_matches('"').replace('\\', "/"));
    if let Some((_, after_execroot)) = path.rsplit_once("/execroot/")
        && let Some((_, relative)) = after_execroot.split_once('/')
    {
        return relative.to_owned();
    }
    path.strip_prefix("./").unwrap_or(&path).to_owned()
}

#[derive(Debug, Default)]
struct SwcParseOutput {
    diagnostics: Vec<Diagnostic>,
    consumed_lines: BTreeSet<String>,
}

fn parse_swc_diagnostics(input: &str) -> SwcParseOutput {
    const MAX_HEADER_DISTANCE: usize = 3;
    const MAX_FRAME_LINES: usize = 8;

    let lines = input.lines().collect::<Vec<_>>();
    let mut output = SwcParseOutput::default();
    for (location_index, line) in lines.iter().enumerate() {
        let Some(location) = parse_swc_source_frame_location(line) else {
            continue;
        };
        let Some((header_index, severity, message)) = lines[..location_index]
            .iter()
            .enumerate()
            .rev()
            .take(MAX_HEADER_DISTANCE)
            .find_map(|(index, line)| {
                parse_swc_message_header(line).map(|(severity, message)| (index, severity, message))
            })
        else {
            continue;
        };

        let mut frame_end = location_index;
        let mut source_line = None;
        let mut first_source_line = None;
        let mut caret_line = None;
        for (index, frame_line) in lines
            .iter()
            .enumerate()
            .skip(location_index + 1)
            .take(MAX_FRAME_LINES)
        {
            if parse_swc_message_header(frame_line).is_some()
                || parse_swc_source_frame_location(frame_line).is_some()
            {
                break;
            }
            frame_end = index;
            output.consumed_lines.insert(frame_line.trim().to_owned());
            if let Some(line_number) = swc_source_line_number(frame_line) {
                let bounded = bounded_text(frame_line.trim(), 256);
                first_source_line.get_or_insert_with(|| bounded.clone());
                if location.line == Some(line_number) {
                    source_line = Some(bounded);
                }
            } else if caret_line.is_none() && is_swc_caret_line(frame_line) {
                caret_line = Some(bounded_text(frame_line.trim(), 256));
            }
            if is_swc_frame_terminator(frame_line) {
                break;
            }
        }

        output
            .consumed_lines
            .insert(lines[header_index].trim().to_owned());
        output
            .consumed_lines
            .insert(lines[location_index].trim().to_owned());
        let (context, context_lines) = swc_failure_context(&lines, header_index, frame_end);
        for context_line in context_lines {
            output
                .consumed_lines
                .insert(lines[context_line].trim().to_owned());
        }

        let mut message = bounded_text(message, 1_024);
        if let Some(context) = context {
            message.push_str("\nSWC context: ");
            message.push_str(&bounded_text(&context, 512));
        }
        if let Some(source_line) = source_line.or(first_source_line) {
            message.push('\n');
            message.push_str(&source_line);
        }
        if let Some(caret_line) = caret_line {
            message.push('\n');
            message.push_str(&caret_line);
        }
        output.diagnostics.push(Diagnostic {
            severity,
            category: DiagnosticCategory::Compilation,
            message,
            location: Some(location),
            target: None,
            action: None,
            repetition_count: 1,
        });
    }
    output
}

fn parse_swc_message_header(line: &str) -> Option<(Severity, &str)> {
    let line = line.trim();
    let (severity, message) = if let Some(message) = line.strip_prefix("error:") {
        (Severity::Error, message)
    } else if let Some(message) = line.strip_prefix("warning:") {
        (Severity::Warning, message)
    } else if let Some(message) = strip_swc_icon(line, 'x').or_else(|| strip_swc_icon(line, '×')) {
        (Severity::Error, message)
    } else {
        (Severity::Warning, strip_swc_icon(line, '!')?)
    };
    let message = message.trim();
    (!message.is_empty()).then_some((severity, message))
}

fn strip_swc_icon(line: &str, icon: char) -> Option<&str> {
    let remainder = line.strip_prefix(icon)?;
    remainder
        .chars()
        .next()
        .is_some_and(char::is_whitespace)
        .then(|| remainder.trim_start())
}

fn parse_swc_source_frame_location(line: &str) -> Option<DiagnosticLocation> {
    let line = line.trim();
    let opening = line.find('[')?;
    let marker = line[..opening].trim_end();
    if !marker.ends_with(",-") && !marker.ends_with("╭─") {
        return None;
    }
    let closing = line[opening + 1..].find(']')? + opening + 1;
    let coordinates = &line[opening + 1..closing];
    let (path_and_line, column) = coordinates.rsplit_once(':')?;
    let (path, line_number) = path_and_line.rsplit_once(':')?;
    if path.trim().is_empty() {
        return None;
    }
    Some(DiagnosticLocation {
        path: compact_javascript_path(path.trim()),
        line: Some(line_number.parse::<u32>().ok()?),
        column: Some(column.parse::<u32>().ok()?),
    })
}

fn swc_source_line_number(line: &str) -> Option<u32> {
    let line = line.trim();
    let (line_number, _) = line.split_once('|').or_else(|| line.split_once('│'))?;
    let line_number = line_number.trim();
    (!line_number.is_empty() && line_number.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| line_number.parse::<u32>().ok())
        .flatten()
}

fn is_swc_caret_line(line: &str) -> bool {
    let line = line.trim();
    line.contains('^')
        && (line.starts_with(':')
            || line
                .split_once('|')
                .is_some_and(|(prefix, _)| prefix.trim().is_empty()))
}

fn is_swc_frame_terminator(line: &str) -> bool {
    let line = line.trim();
    (line.starts_with('`') && line.contains("---")) || (line.starts_with('╰') && line.contains('─'))
}

fn swc_failure_context(
    lines: &[&str],
    header_index: usize,
    frame_end: usize,
) -> (Option<String>, Vec<usize>) {
    const MAX_CONTEXT_DISTANCE: usize = 12;
    const MAX_CONTEXT_ITEMS: usize = 2;

    let mut contexts = Vec::new();
    let mut consumed = Vec::new();
    let preceding_start = header_index.saturating_sub(MAX_CONTEXT_DISTANCE);
    for (index, line) in lines
        .iter()
        .enumerate()
        .take(header_index)
        .skip(preceding_start)
    {
        if let Some(context) = parse_swc_context_line(line)
            && !contexts.contains(&context)
        {
            contexts.push(context);
            consumed.push(index);
        }
        if contexts.len() >= MAX_CONTEXT_ITEMS {
            break;
        }
    }
    for (index, line) in lines
        .iter()
        .enumerate()
        .skip(frame_end + 1)
        .take(MAX_CONTEXT_DISTANCE)
    {
        if parse_swc_message_header(line).is_some()
            || parse_swc_source_frame_location(line).is_some()
        {
            break;
        }
        if let Some(context) = parse_swc_context_line(line)
            && !contexts.contains(&context)
        {
            contexts.push(context);
            consumed.push(index);
        }
        if contexts.len() >= MAX_CONTEXT_ITEMS {
            break;
        }
    }
    contexts.truncate(MAX_CONTEXT_ITEMS);
    (
        (!contexts.is_empty()).then(|| contexts.join("; ")),
        consumed,
    )
}

fn parse_swc_context_line(line: &str) -> Option<String> {
    if parse_swc_message_header(line).is_some() {
        return None;
    }
    let mut line = line.trim();
    if let Some(remainder) = line.strip_prefix("Caused by:") {
        line = remainder.trim();
    }
    if let Some((number, remainder)) = line.split_once(": ")
        && !number.is_empty()
        && number.bytes().all(|byte| byte.is_ascii_digit())
    {
        line = remainder.trim();
    }
    let lower = line.to_ascii_lowercase();
    let is_context = lower.starts_with("failed to process")
        || lower.starts_with("failed to parse")
        || lower.starts_with("failed to transform")
        || lower.starts_with("failed to invoke plugin")
        || lower.starts_with("failed to handle")
        || (lower.contains("plugin") && lower.contains("failed"));
    (is_context && !line.is_empty()).then(|| line.to_owned())
}

fn is_swc_action_wrapper(line: &str) -> bool {
    let line = line.trim();
    let lower = line.to_ascii_lowercase();
    line.starts_with("ERROR:")
        && (lower.contains("build did not complete successfully")
            || lower.contains("failed: error executing")
            || (lower.contains("swc")
                && (lower.contains("failed:") || lower.contains("action failed"))))
}

fn parse_typescript_diagnostic(line: &str) -> Option<Diagnostic> {
    let line = line
        .trim()
        .strip_prefix("ERROR: ")
        .unwrap_or_else(|| line.trim());
    let (path, line_number, column, message) = parse_typescript_parenthesized_location(line)
        .or_else(|| parse_typescript_pretty_location(line))?;
    let message = message.trim().trim_start_matches('-').trim();
    let (severity, message) = if let Some(message) = message.strip_prefix("error ") {
        (Severity::Error, message)
    } else {
        (Severity::Warning, message.strip_prefix("warning ")?)
    };
    let (code, message) = message.split_once(':')?;
    if !code.strip_prefix("TS").is_some_and(|digits| {
        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
    }) {
        return None;
    }
    let message = message.trim();
    if message.is_empty() {
        return None;
    }
    Some(Diagnostic {
        severity,
        category: DiagnosticCategory::Compilation,
        message: format!("{code}: {message}"),
        location: Some(DiagnosticLocation {
            path: compact_javascript_path(path),
            line: Some(line_number),
            column: Some(column),
        }),
        target: None,
        action: None,
        repetition_count: 1,
    })
}

fn parse_typescript_parenthesized_location(line: &str) -> Option<(&str, u32, u32, &str)> {
    let path_end = typescript_path_end(line, '(')?;
    let remainder = line[path_end..].strip_prefix('(')?;
    let (coordinates, message) = remainder.split_once("): ")?;
    let (line_number, column) = coordinates.split_once(',')?;
    let line_number = line_number.trim().parse::<u32>().ok()?;
    let column = column.trim().parse::<u32>().ok()?;
    Some((&line[..path_end], line_number, column, message))
}

fn parse_typescript_pretty_location(line: &str) -> Option<(&str, u32, u32, &str)> {
    let path_end = typescript_path_end(line, ':')?;
    let (line_number, remainder) = line[path_end + 1..].split_once(':')?;
    let line_number = line_number.parse::<u32>().ok()?;
    let (column, message) = remainder
        .split_once(" - ")
        .or_else(|| remainder.split_once(':'))?;
    let column = column.parse::<u32>().ok()?;
    Some((&line[..path_end], line_number, column, message))
}

fn typescript_path_end(line: &str, delimiter: char) -> Option<usize> {
    const EXTENSIONS: [&str; 8] = [".tsx", ".mts", ".cts", ".ts", ".jsx", ".mjs", ".cjs", ".js"];
    EXTENSIONS
        .iter()
        .filter_map(|extension| {
            let marker = format!("{extension}{delimiter}");
            line.rfind(&marker).map(|index| index + extension.len())
        })
        .max()
}

/// Stateful extractor for Node.js exceptions and their application frames.
#[derive(Debug, Default)]
pub struct JavaScriptTestDiagnosticParser {
    leading_location: Option<DiagnosticLocation>,
    pending: Option<Diagnostic>,
    frames_seen: usize,
}

impl JavaScriptTestDiagnosticParser {
    const MAX_STACK_FRAMES: usize = 64;

    /// Observes one normalized test-log line and emits an exception after a
    /// JavaScript source header or application stack frame confirms it.
    pub fn observe_line(&mut self, line: &str) -> Option<Diagnostic> {
        if !line.trim_start().starts_with("at ")
            && let Some(location) = parse_javascript_location(line.trim())
        {
            let previous = self.take_confirmed();
            self.leading_location = Some(location);
            return previous;
        }
        if let Some(message) = javascript_exception_message(line) {
            let leading_location = self.leading_location.take();
            let previous = self.take_confirmed();
            self.pending = Some(Diagnostic {
                severity: Severity::Error,
                category: DiagnosticCategory::Test,
                message: message.to_owned(),
                location: leading_location,
                target: None,
                action: None,
                repetition_count: 1,
            });
            self.frames_seen = 0;
            return previous;
        }
        self.pending.as_ref()?;
        if let Some(location) = parse_javascript_stack_frame(line) {
            self.frames_seen = self.frames_seen.saturating_add(1);
            if let Some(location) = location {
                let mut diagnostic = self.pending.take()?;
                diagnostic.location = Some(location);
                self.frames_seen = 0;
                return Some(diagnostic);
            }
            if self.frames_seen >= Self::MAX_STACK_FRAMES {
                return self.take_confirmed();
            }
            return None;
        }
        if line.trim().is_empty() {
            return None;
        }
        self.take_confirmed()
    }

    /// Emits a confirmed exception that reached end-of-file.
    pub fn finish(&mut self) -> Option<Diagnostic> {
        self.take_confirmed()
    }

    fn take_confirmed(&mut self) -> Option<Diagnostic> {
        let confirmed = self
            .pending
            .as_ref()
            .is_some_and(|diagnostic| diagnostic.location.is_some())
            || self.frames_seen > 0;
        self.frames_seen = 0;
        self.leading_location = None;
        if confirmed {
            self.pending.take()
        } else {
            self.pending = None;
            None
        }
    }
}

fn javascript_exception_message(line: &str) -> Option<&str> {
    let line = line.trim();
    let exception_type = line.split_once(':').map_or(line, |(name, _)| name);
    let class_name = exception_type.split_whitespace().next()?;
    (!class_name.is_empty()
        && class_name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'$'))
        && (class_name.ends_with("Error") || class_name.ends_with("Exception")))
    .then_some(line)
}

fn parse_javascript_stack_frame(line: &str) -> Option<Option<DiagnosticLocation>> {
    let frame = line.trim().strip_prefix("at ")?;
    let source = if let Some((_, source)) = frame.rsplit_once('(') {
        source.strip_suffix(')')?
    } else {
        frame.split_whitespace().last()?
    };
    if source.starts_with("node:") || matches!(source, "native" | "<anonymous>") {
        return Some(None);
    }
    let location = parse_javascript_location(source)?;
    let framework = location.path.contains("/node_modules/")
        || location.path.starts_with("node_modules/")
        || location.path.contains("/external/");
    Some((!framework).then_some(location))
}

fn parse_javascript_location(value: &str) -> Option<DiagnosticLocation> {
    let value = value.trim().trim_matches('"');
    let path_end = javascript_path_end(value)?;
    let coordinates = value[path_end..].strip_prefix(':')?;
    let (line_number, column) = if let Some((line_number, column)) = coordinates.split_once(':') {
        (
            line_number.parse::<u32>().ok()?,
            Some(column.parse::<u32>().ok()?),
        )
    } else {
        (coordinates.parse::<u32>().ok()?, None)
    };
    Some(DiagnosticLocation {
        path: compact_javascript_path(&value[..path_end]),
        line: Some(line_number),
        column,
    })
}

fn javascript_path_end(value: &str) -> Option<usize> {
    const EXTENSIONS: [&str; 8] = [".tsx", ".mts", ".cts", ".ts", ".jsx", ".mjs", ".cjs", ".js"];
    EXTENSIONS
        .iter()
        .filter_map(|extension| {
            let marker = format!("{extension}:");
            value.rfind(&marker).map(|index| index + extension.len())
        })
        .max()
}

fn compact_javascript_path(path: &str) -> String {
    let path = path
        .trim_matches('"')
        .strip_prefix("file://")
        .unwrap_or(path)
        .replace('\\', "/");
    let path = strip_workspace_marker(path);
    for marker in [".runfiles/_main/", ".runfiles/__main__/"] {
        if let Some((_, relative)) = path.rsplit_once(marker) {
            return relative.to_owned();
        }
    }
    if let Some((_, after_execroot)) = path.rsplit_once("/execroot/")
        && let Some((_, relative)) = after_execroot.split_once('/')
    {
        return relative.to_owned();
    }
    path.strip_prefix("./").unwrap_or(&path).to_owned()
}

fn parse_protobuf_diagnostic(line: &str) -> Option<Diagnostic> {
    let marker = line.rfind(".proto:")?;
    let path_end = marker + ".proto".len();
    let path = line[..path_end]
        .trim()
        .strip_prefix("ERROR: ")
        .unwrap_or_else(|| line[..path_end].trim());
    let (line_number, remainder) = split_u32_prefix(&line[path_end + 1..])?;
    let (column, message) = split_u32_prefix(remainder)
        .map_or((None, remainder), |(column, message)| {
            (Some(column), message)
        });
    let message = message.trim();
    let (severity, message) = if let Some(message) = message.strip_prefix("warning:") {
        (Severity::Warning, message.trim())
    } else if let Some(message) = message.strip_prefix("error:") {
        (Severity::Error, message.trim())
    } else {
        (Severity::Error, message)
    };
    if message.is_empty() {
        return None;
    }
    Some(Diagnostic {
        severity,
        category: DiagnosticCategory::Compilation,
        message: message.to_owned(),
        location: Some(DiagnosticLocation {
            path: compact_protobuf_path(path),
            line: Some(line_number),
            column,
        }),
        target: None,
        action: None,
        repetition_count: 1,
    })
}

fn compact_protobuf_path(path: &str) -> String {
    let path = strip_workspace_marker(path.trim_matches('"').replace('\\', "/"));
    if let Some((_, after_execroot)) = path.rsplit_once("/execroot/")
        && let Some((_, relative)) = after_execroot.split_once('/')
    {
        return relative.to_owned();
    }
    path.strip_prefix("./").unwrap_or(&path).to_owned()
}

fn parse_java_compiler_diagnostics(input: &str) -> Vec<Diagnostic> {
    const MAX_CONTEXT_LINES: usize = 8;
    let lines = input.lines().collect::<Vec<_>>();
    let mut diagnostics = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let Some(mut diagnostic) = parse_java_compiler_diagnostic(line) else {
            continue;
        };
        if diagnostic
            .message
            .eq_ignore_ascii_case("cannot find symbol")
        {
            for context in lines.iter().skip(index + 1).take(MAX_CONTEXT_LINES) {
                if parse_java_compiler_diagnostic(context).is_some()
                    || context.trim_start().starts_with("ERROR:")
                {
                    break;
                }
                if let Some(symbol) = context.trim().strip_prefix("symbol:") {
                    diagnostic.message = format!(
                        "cannot find symbol: {}",
                        symbol.split_whitespace().collect::<Vec<_>>().join(" ")
                    );
                    break;
                }
            }
        }
        diagnostics.push(diagnostic);
    }
    diagnostics
}

fn parse_java_compiler_diagnostic(line: &str) -> Option<Diagnostic> {
    let marker = line.rfind(".java:")?;
    let path_end = marker + ".java".len();
    let path = line[..path_end]
        .trim()
        .strip_prefix("ERROR: ")
        .unwrap_or_else(|| line[..path_end].trim());
    let (line_number, remainder) = split_u32_prefix(&line[path_end + 1..])?;
    let (column, message) = split_u32_prefix(remainder)
        .map_or((None, remainder), |(column, message)| {
            (Some(column), message)
        });
    let message = message.trim();
    let (severity, message) = if let Some(message) = message.strip_prefix("error:") {
        (Severity::Error, message.trim())
    } else {
        let message = message.strip_prefix("warning:")?;
        (Severity::Warning, message.trim())
    };
    if message.is_empty() {
        return None;
    }
    Some(Diagnostic {
        severity,
        category: DiagnosticCategory::Compilation,
        message: message.to_owned(),
        location: Some(DiagnosticLocation {
            path: compact_java_path(path),
            line: Some(line_number),
            column,
        }),
        target: None,
        action: None,
        repetition_count: 1,
    })
}

/// Stateful extractor for Java exceptions followed by JVM stack frames.
#[derive(Debug, Default)]
pub struct JavaTestDiagnosticParser {
    pending: Option<Diagnostic>,
    pending_is_explicit: bool,
    frames_seen: usize,
}

impl JavaTestDiagnosticParser {
    const MAX_STACK_FRAMES: usize = 64;

    /// Observes one normalized test-log line and emits an exception once an
    /// application frame, another exception, or the end of its stack is seen.
    pub fn observe_line(&mut self, line: &str) -> Option<Diagnostic> {
        if let Some((message, explicitly_java)) = parse_java_exception_line(line) {
            let previous = self.take_confirmed();
            self.pending = Some(Diagnostic {
                severity: Severity::Error,
                category: DiagnosticCategory::Test,
                message: message.to_owned(),
                location: None,
                target: None,
                action: None,
                repetition_count: 1,
            });
            self.pending_is_explicit = explicitly_java;
            self.frames_seen = 0;
            return previous;
        }
        self.pending.as_ref()?;
        if let Some((location, framework_frame)) = parse_java_stack_frame(line) {
            self.frames_seen = self.frames_seen.saturating_add(1);
            if !framework_frame {
                let mut diagnostic = self.pending.take()?;
                diagnostic.location = Some(location);
                self.pending_is_explicit = false;
                self.frames_seen = 0;
                return Some(diagnostic);
            }
            if self.frames_seen >= Self::MAX_STACK_FRAMES {
                return self.take_confirmed();
            }
            return None;
        }
        if line.trim().is_empty() {
            return None;
        }
        self.take_confirmed()
    }

    /// Emits an exception that reached end-of-file without an application frame.
    pub fn finish(&mut self) -> Option<Diagnostic> {
        self.take_confirmed()
    }

    fn take_confirmed(&mut self) -> Option<Diagnostic> {
        let confirmed = self.pending_is_explicit || self.frames_seen > 0;
        self.pending_is_explicit = false;
        self.frames_seen = 0;
        if confirmed {
            self.pending.take()
        } else {
            self.pending = None;
            None
        }
    }
}

fn java_exception_message(line: &str) -> Option<&str> {
    parse_java_exception_line(line).map(|(message, _)| message)
}

fn parse_java_exception_line(line: &str) -> Option<(&str, bool)> {
    let mut line = line.trim();
    let mut explicitly_java = false;
    if let Some(remainder) = line.strip_prefix("Exception in thread \"") {
        let (_, remainder) = remainder.split_once("\" ")?;
        line = remainder;
        explicitly_java = true;
    } else if let Some(remainder) = line.strip_prefix("Caused by: ") {
        line = remainder;
        explicitly_java = true;
    }
    let exception_type = line.split_once(':').map_or(line, |(name, _)| name);
    if exception_type.is_empty()
        || exception_type
            .bytes()
            .any(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'$')))
    {
        return None;
    }
    let class_name = exception_type.rsplit('.').next()?;
    let recognized = class_name.ends_with("Error")
        || class_name.ends_with("Exception")
        || class_name.ends_with("Failure");
    (recognized && (explicitly_java || exception_type.contains('.')))
        .then_some((line, explicitly_java))
}

fn parse_java_stack_frame(line: &str) -> Option<(DiagnosticLocation, bool)> {
    let frame = line.trim().strip_prefix("at ")?;
    let (callable, source) = frame.split_once('(')?;
    let source = source.strip_suffix(')')?;
    let (file, line_number) = source.rsplit_once(':')?;
    if !file.ends_with(".java") {
        return None;
    }
    let line_number = line_number.parse::<u32>().ok()?;
    let callable = callable.rsplit_once('/').map_or(callable, |(_, name)| name);
    let class_name = callable.rsplit_once('.')?.0;
    let package = class_name.rsplit_once('.').map(|(package, _)| package);
    let path = package.map_or_else(
        || file.to_owned(),
        |package| format!("{}/{}", package.replace('.', "/"), file),
    );
    let framework_frame = [
        "java.",
        "javax.",
        "jdk.",
        "sun.",
        "junit.",
        "org.junit.",
        "org.hamcrest.",
        "org.opentest4j.",
        "com.google.testing.junit.",
    ]
    .iter()
    .any(|prefix| callable.starts_with(prefix));
    Some((
        DiagnosticLocation {
            path,
            line: Some(line_number),
            column: None,
        },
        framework_frame,
    ))
}

fn compact_java_path(path: &str) -> String {
    let path = strip_workspace_marker(path.trim_matches('"').replace('\\', "/"));
    if let Some((_, after_execroot)) = path.rsplit_once("/execroot/")
        && let Some((_, relative)) = after_execroot.split_once('/')
    {
        return relative.to_owned();
    }
    path.strip_prefix("./").unwrap_or(&path).to_owned()
}

/// Stateful extractor for Bazel's Starlark source and traceback diagnostics.
///
/// Syntax diagnostics carry their location inline. Runtime Starlark failures
/// instead print one or more `File "...", line N, column N` frames before a
/// terminal `Error in ...` line, so only the latest frame must be retained.
#[derive(Debug)]
struct StarlarkDiagnosticParser {
    location: Option<DiagnosticLocation>,
    category: DiagnosticCategory,
}

impl Default for StarlarkDiagnosticParser {
    fn default() -> Self {
        Self {
            location: None,
            category: DiagnosticCategory::Loading,
        }
    }
}

impl StarlarkDiagnosticParser {
    fn observe_line(&mut self, line: &str) -> Option<Diagnostic> {
        if is_starlark_traceback_header(line) {
            self.location = None;
            if line.trim_start().starts_with("ERROR:") {
                self.category = DiagnosticCategory::Loading;
            }
            return None;
        }
        if let Some(diagnostic) = parse_starlark_inline_diagnostic(line) {
            self.location = diagnostic.location.clone();
            self.category = diagnostic.category;
            return is_starlark_root_cause_message(&diagnostic.message).then_some(diagnostic);
        }
        if let Some(location) = parse_starlark_traceback_location(line) {
            self.location = Some(location);
            return None;
        }
        let message = starlark_error_message(line)?;
        Some(Diagnostic {
            severity: Severity::Error,
            category: self.category,
            message: message.to_owned(),
            location: self.location.take(),
            target: None,
            action: None,
            repetition_count: 1,
        })
    }
}

fn parse_starlark_inline_diagnostic(line: &str) -> Option<Diagnostic> {
    let line = line
        .trim()
        .strip_prefix("ERROR: ")
        .unwrap_or_else(|| line.trim());
    let path_end = starlark_path_end(line)?;
    let path = &line[..path_end];
    let (line_number, remainder) = split_u32_prefix(&line[path_end + 1..])?;
    let (column, message) = split_u32_prefix(remainder)
        .map_or((None, remainder), |(column, message)| {
            (Some(column), message)
        });
    let message = message.trim();
    if message.is_empty() {
        return None;
    }
    let lower = message.to_ascii_lowercase();
    let category = if (lower.starts_with("in ") && lower.contains(" rule //"))
        || lower.contains("analysis of target")
        || lower.contains("aspect on target")
    {
        DiagnosticCategory::Analysis
    } else {
        DiagnosticCategory::Loading
    };
    Some(Diagnostic {
        severity: if lower.contains("warning:") {
            Severity::Warning
        } else {
            Severity::Error
        },
        category,
        message: message.to_owned(),
        location: Some(DiagnosticLocation {
            path: compact_starlark_path(path),
            line: Some(line_number),
            column,
        }),
        target: None,
        action: None,
        repetition_count: 1,
    })
}

fn starlark_path_end(line: &str) -> Option<usize> {
    const MARKERS: [&str; 6] = [
        ".bzl:",
        ".bazel:",
        "/BUILD:",
        "\\BUILD:",
        "/WORKSPACE:",
        "\\WORKSPACE:",
    ];
    MARKERS
        .iter()
        .filter_map(|marker| {
            line.rfind(marker)
                .map(|index| index + marker.len().saturating_sub(1))
        })
        .max()
}

fn parse_starlark_traceback_location(line: &str) -> Option<DiagnosticLocation> {
    let marker = "File \"";
    let start = line.find(marker)? + marker.len();
    let remainder = &line[start..];
    let (path, remainder) = remainder.split_once("\", line ")?;
    if !is_starlark_path(path) {
        return None;
    }
    let line_digits = remainder
        .bytes()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if line_digits == 0 {
        return None;
    }
    let line_number = remainder[..line_digits].parse::<u32>().ok()?;
    let column = remainder[line_digits..]
        .strip_prefix(", column ")
        .and_then(|remainder| {
            let digits = remainder
                .bytes()
                .take_while(|byte| byte.is_ascii_digit())
                .count();
            (digits > 0)
                .then(|| remainder[..digits].parse::<u32>().ok())
                .flatten()
        });
    Some(DiagnosticLocation {
        path: compact_starlark_path(path),
        line: Some(line_number),
        column,
    })
}

fn is_starlark_path(path: &str) -> bool {
    path.ends_with(".bzl")
        || path.ends_with(".bazel")
        || matches!(path.rsplit(['/', '\\']).next(), Some("BUILD" | "WORKSPACE"))
}

fn is_starlark_traceback_header(line: &str) -> bool {
    line.trim()
        .strip_prefix("ERROR: ")
        .unwrap_or_else(|| line.trim())
        == "Traceback (most recent call last):"
}

fn starlark_error_message(line: &str) -> Option<&str> {
    let line = line.trim();
    (line.starts_with("Error in ") || line.starts_with("Error: ")).then_some(line)
}

fn is_starlark_root_cause_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("syntax error")
        || lower.contains("contains syntax errors")
        || (lower.contains("name '") && lower.contains(" is not defined"))
}

fn compact_starlark_path(path: &str) -> String {
    let path = strip_workspace_marker(path.trim_matches('"').replace('\\', "/"));
    if let Some((_, after_execroot)) = path.rsplit_once("/execroot/")
        && let Some((_, relative)) = after_execroot.split_once('/')
    {
        return relative.to_owned();
    }
    path.strip_prefix("./").unwrap_or(&path).to_owned()
}

/// Stateful extractor for standard Python traceback and syntax-error output.
///
/// Python reports source locations on a `File "...", line N` frame before the
/// terminal exception. Keeping only the latest frame is bounded and matches
/// traceback semantics, where the innermost frame is printed last.
#[derive(Debug, Default)]
pub struct PythonDiagnosticParser {
    location: Option<DiagnosticLocation>,
}

impl PythonDiagnosticParser {
    /// Observes one normalized output line and returns a diagnostic when the
    /// line terminates a Python exception block.
    pub fn observe_line(&mut self, line: &str) -> Option<Diagnostic> {
        if line.trim() == "Traceback (most recent call last):" {
            self.location = None;
            return None;
        }
        if let Some(location) = parse_python_location(line) {
            self.location = Some(location);
            return None;
        }
        let message = python_exception_message(line)?;
        let exception_type = message.split_once(':').map_or(message, |(name, _)| name);
        Some(Diagnostic {
            severity: if exception_type.ends_with("Warning") {
                Severity::Warning
            } else {
                Severity::Error
            },
            category: DiagnosticCategory::Compilation,
            message: message.to_owned(),
            location: self.location.take(),
            target: None,
            action: None,
            repetition_count: 1,
        })
    }
}

fn parse_python_location(line: &str) -> Option<DiagnosticLocation> {
    let marker = "File \"";
    let start = line.find(marker)? + marker.len();
    let remainder = &line[start..];
    let (path, remainder) = remainder.split_once("\", line ")?;
    let synthetic_path = path.starts_with('<')
        && !path.starts_with("<RUNTIME_ROOT>/")
        && !path.starts_with("<WORKSPACE>/")
        && !path.starts_with("<workspace>/");
    if synthetic_path || path.ends_with("_stage2_bootstrap.py") {
        return None;
    }
    let digits = remainder
        .bytes()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if digits == 0 {
        return None;
    }
    let line_number = remainder[..digits].parse::<u32>().ok()?;
    Some(DiagnosticLocation {
        path: compact_python_path(path),
        line: Some(line_number),
        column: None,
    })
}

fn python_exception_message(line: &str) -> Option<&str> {
    let mut line = line.trim();
    if let Some(remainder) = line.strip_prefix('E')
        && remainder.chars().next().is_some_and(char::is_whitespace)
    {
        line = remainder.trim_start();
    }
    if line.contains("File \"") {
        return None;
    }
    let exception_type = line.split_once(':').map_or(line, |(name, _)| name);
    if exception_type.is_empty()
        || exception_type
            .bytes()
            .any(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.')))
    {
        return None;
    }
    let class_name = exception_type.rsplit('.').next()?;
    let recognized = class_name.ends_with("Error")
        || class_name.ends_with("Exception")
        || class_name.ends_with("Failure")
        || class_name.ends_with("Warning")
        || matches!(class_name, "Failed" | "KeyboardInterrupt" | "SystemExit");
    recognized.then_some(line)
}

fn compact_python_path(path: &str) -> String {
    let path = strip_workspace_marker(path.trim_matches('"').replace('\\', "/"));
    for marker in [".runfiles/_main/", ".runfiles/__main__/"] {
        if let Some((_, relative)) = path.rsplit_once(marker) {
            return relative.to_owned();
        }
    }
    if let Some((_, after_execroot)) = path.rsplit_once("/execroot/")
        && let Some((_, relative)) = after_execroot.split_once('/')
    {
        return relative.to_owned();
    }
    path.strip_prefix("./").unwrap_or(&path).to_owned()
}

/// Parses the standard Go compiler location form without depending on a
/// particular diagnostic message or language setting.
#[must_use]
pub fn parse_go_diagnostic(line: &str) -> Option<Diagnostic> {
    let marker = line.rfind(".go:")?;
    let path_end = marker + ".go".len();
    let path = line[..path_end]
        .trim()
        .strip_prefix("ERROR: ")
        .unwrap_or_else(|| line[..path_end].trim());
    let (line_number, remainder) = split_u32_prefix(&line[path_end + 1..])?;
    let (column, message) = split_u32_prefix(remainder)
        .map_or((None, remainder), |(column, message)| {
            (Some(column), message)
        });
    let message = message.trim();
    if message.is_empty() {
        return None;
    }
    Some(Diagnostic {
        severity: if message.to_ascii_lowercase().contains("warning:") {
            Severity::Warning
        } else {
            Severity::Error
        },
        category: DiagnosticCategory::Compilation,
        message: message.to_owned(),
        location: Some(DiagnosticLocation {
            path: compact_go_path(path),
            line: Some(line_number),
            column,
        }),
        target: None,
        action: None,
        repetition_count: 1,
    })
}

fn split_u32_prefix(value: &str) -> Option<(u32, &str)> {
    let (number, remainder) = value.split_once(':')?;
    let number = number.trim();
    (!number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| number.parse::<u32>().ok().map(|number| (number, remainder)))
        .flatten()
}

fn strict_dependency_diagnostic(line: &str) -> Option<Diagnostic> {
    const MARKER: &str = ": import of \"";
    let marker = line.find(MARKER)?;
    let path = line[..marker].trim();
    if !path.ends_with(".go") {
        return None;
    }
    let import = line[marker + MARKER.len()..].split('"').next()?.trim();
    if import.is_empty() {
        return None;
    }
    let path = compact_go_path(path);
    Some(Diagnostic {
        severity: Severity::Error,
        category: DiagnosticCategory::Compilation,
        message: format!(
            "missing strict dependency: {path} imports \"{import}\"; add its target to deps"
        ),
        location: Some(DiagnosticLocation {
            path,
            line: None,
            column: None,
        }),
        target: None,
        action: None,
        repetition_count: 1,
    })
}

fn compact_go_path(path: &str) -> String {
    let path = strip_workspace_marker(path.trim_matches('"').replace('\\', "/"));
    if let Some((_, after_execroot)) = path.rsplit_once("/execroot/")
        && let Some((_, relative)) = after_execroot.split_once('/')
    {
        return relative.to_owned();
    }
    path
}

fn strip_workspace_marker(path: String) -> String {
    path.strip_prefix("<WORKSPACE>/")
        .or_else(|| path.strip_prefix("<workspace>/"))
        .unwrap_or(&path)
        .to_owned()
}

fn is_actionable(line: &str) -> bool {
    let line = line.trim();
    let lower = line.to_ascii_lowercase();
    if matches!(lower.as_str(), "failure:" | "failures:")
        || (line.starts_with("test ") && lower.ends_with(" ... ok"))
    {
        return false;
    }
    lower.contains("error:")
        || lower.starts_with("error ")
        || lower.contains("failed:")
        || lower.contains("no such target")
        || lower.contains("no such package")
        || lower.contains("visibility error")
        || lower.contains("undefined reference")
        || lower.contains("fatal:")
        || lower.contains("root_cause")
        || lower.contains("panicked at")
        || (lower.contains("assertion") && lower.contains(" failed"))
        || lower.starts_with("test result: failed")
        || (line.starts_with("test ") && line.ends_with(" ... FAILED"))
}

fn category_from_text(line: &str) -> DiagnosticCategory {
    let lower = line.to_ascii_lowercase();
    if lower.contains("no such package") || lower.contains("no such target") {
        DiagnosticCategory::Loading
    } else if lower.contains("visibility") {
        DiagnosticCategory::Visibility
    } else if lower.contains("analysis") {
        DiagnosticCategory::Analysis
    } else if lower.contains("test") || lower.contains("panicked at") || lower.contains("assertion")
    {
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
    fn recognizes_go_compiler_diagnostics_without_error_markers() {
        let summary = reduce_invocation(ReductionInput {
            events: &[],
            stdout: b"",
            stderr: b"ERROR: Build did NOT complete successfully\nconfig/config.go:12:40: cannot use 42 (untyped int constant) as string value in variable declaration\n",
            exit_code: Some(1),
            elapsed_ms: 1,
            budget: Budget::result_default(),
        });

        let diagnostic = &summary.diagnostics[0];
        assert_eq!(diagnostic.category, DiagnosticCategory::Compilation);
        assert_eq!(
            diagnostic.message,
            "cannot use 42 (untyped int constant) as string value in variable declaration"
        );
        assert_eq!(
            diagnostic.location,
            Some(DiagnosticLocation {
                path: "config/config.go".into(),
                line: Some(12),
                column: Some(40),
            })
        );
        assert!(summary.headline.contains("cannot use 42"));
    }

    #[test]
    fn reduces_rules_go_strict_dependency_blocks_to_the_offending_import() {
        let stderr = br#"ERROR: GoCompilePkg config/config.a failed
compilepkg: missing strict dependencies:
/private/tmp/_bazel_user/hash/sandbox/darwin-sandbox/4/execroot/_main/config/config.go: import of "github.com/hashicorp/go-version"
No dependencies were provided.
Check that imports in Go sources match importpath attributes in deps.
ERROR: Build did NOT complete successfully
"#;
        let summary = reduce_invocation(ReductionInput {
            events: &[],
            stdout: b"",
            stderr,
            exit_code: Some(1),
            elapsed_ms: 1,
            budget: Budget::result_default(),
        });

        let diagnostic = &summary.diagnostics[0];
        assert_eq!(diagnostic.category, DiagnosticCategory::Compilation);
        assert_eq!(
            diagnostic.message,
            "missing strict dependency: config/config.go imports \"github.com/hashicorp/go-version\"; add its target to deps"
        );
        assert_eq!(
            diagnostic.location,
            Some(DiagnosticLocation {
                path: "config/config.go".into(),
                line: None,
                column: None,
            })
        );
        assert!(summary.headline.contains("missing strict dependency"));
    }

    #[test]
    fn keeps_identical_go_messages_at_distinct_locations() {
        let summary = reduce_invocation(ReductionInput {
            events: &[],
            stdout: b"",
            stderr: b"first.go:3:2: undefined: missing\nsecond.go:7:4: undefined: missing\n",
            exit_code: Some(1),
            elapsed_ms: 1,
            budget: Budget::result_default(),
        });

        assert_eq!(summary.diagnostics.len(), 2);
        assert_eq!(
            summary
                .diagnostics
                .iter()
                .filter_map(|diagnostic| diagnostic.location.as_ref())
                .map(|location| location.path.as_str())
                .collect::<Vec<_>>(),
            vec!["first.go", "second.go"]
        );
    }

    #[test]
    fn structures_clang_cpp_diagnostics_with_source_coordinates() {
        let summary = reduce_invocation(ReductionInput {
            events: &[],
            stdout: b"",
            stderr: b"mcp/cpp_fixture/type_mismatch.cc:5:7: error: no viable conversion from 'std::string' to 'int'\nERROR: Build did NOT complete successfully\n",
            exit_code: Some(1),
            elapsed_ms: 1,
            budget: Budget::result_default(),
        });

        let diagnostic = &summary.diagnostics[0];
        assert_eq!(
            diagnostic.message,
            "no viable conversion from 'std::string' to 'int'"
        );
        assert_eq!(diagnostic.category, DiagnosticCategory::Compilation);
        assert_eq!(
            diagnostic.location,
            Some(DiagnosticLocation {
                path: "mcp/cpp_fixture/type_mismatch.cc".into(),
                line: Some(5),
                column: Some(7),
            })
        );
        assert!(summary.headline.contains("no viable conversion"));
    }

    #[test]
    fn structures_msvc_cpp_diagnostics_and_preserves_error_codes() {
        let diagnostic = parse_cpp_diagnostic(
            r"C:\workspace\mcp\cpp_fixture\type_mismatch.cpp(12,7): error C2440: 'initializing': cannot convert from 'std::string' to 'int'",
        )
        .unwrap();

        assert_eq!(diagnostic.severity, Severity::Error);
        assert_eq!(
            diagnostic.message,
            "C2440: 'initializing': cannot convert from 'std::string' to 'int'"
        );
        assert_eq!(
            diagnostic.location,
            Some(DiagnosticLocation {
                path: "C:/workspace/mcp/cpp_fixture/type_mismatch.cpp".into(),
                line: Some(12),
                column: Some(7),
            })
        );
    }

    #[test]
    fn allocation_free_ascii_matching_preserves_case_insensitive_ranking() {
        assert!(contains_ignore_ascii_case(
            "FANOUT_ROOT_CAUSE",
            "root_cause"
        ));
        assert!(contains_ignore_ascii_case(
            "ERROR EXECUTING CppCompile",
            "error executing"
        ));
        assert!(starts_with_ignore_ascii_case(
            "TEST FAILED: //pkg:test",
            "test failed:"
        ));
        assert!(!starts_with_ignore_ascii_case(
            "xTEST FAILED: //pkg:test",
            "test failed:"
        ));
    }

    #[test]
    fn finds_cpp_path_extensions_without_formatted_markers() {
        assert_eq!(cpp_path_end("generated.cc.tmp.cc:9: error", ':'), Some(19));
        assert_eq!(
            cpp_path_end(r"C:\workspace\source.cpp(12,7): error", '('),
            Some(23)
        );
        assert_eq!(cpp_path_end("source.cc.tmp:9: error", ':'), None);
    }

    #[test]
    fn ranks_apple_undefined_symbols_ahead_of_linker_wrappers() {
        let summary = reduce_invocation(ReductionInput {
            events: &[],
            stdout: b"",
            stderr: br#"Undefined symbols for architecture arm64:
  "calculate_invoice_total(int)", referenced from:
      _main in undefined_reference.o
ld: symbol(s) not found for architecture arm64
clang: error: linker command failed with exit code 1 (use -v to see invocation)
"#,
            exit_code: Some(1),
            elapsed_ms: 1,
            budget: Budget::result_default(),
        });

        assert_eq!(
            summary.diagnostics[0].message,
            "undefined symbol: calculate_invoice_total(int)"
        );
        assert!(summary.headline.contains("calculate_invoice_total"));
    }

    #[test]
    fn parses_gnu_undefined_references_with_source_locations() {
        let diagnostic = parse_cpp_linker_diagnostic(
            "/usr/bin/ld: object.o: /tmp/output/execroot/_main/mcp/cpp_fixture/undefined_reference.cc:3: undefined reference to `calculate_invoice_total(int)'",
        )
        .unwrap();

        assert_eq!(
            diagnostic.message,
            "undefined reference to calculate_invoice_total(int)"
        );
        assert_eq!(
            diagnostic.location,
            Some(DiagnosticLocation {
                path: "mcp/cpp_fixture/undefined_reference.cc".into(),
                line: Some(3),
                column: None,
            })
        );
    }

    #[test]
    fn parses_msvc_unresolved_external_symbols() {
        let diagnostic = parse_cpp_linker_diagnostic(
            r#"undefined_reference.obj : error LNK2019: unresolved external symbol "int __cdecl calculate_invoice_total(int)" referenced in function main"#,
        )
        .unwrap();

        assert_eq!(
            diagnostic.message,
            r#"unresolved external symbol "int __cdecl calculate_invoice_total(int)""#
        );
    }

    #[test]
    fn structures_typescript_compiler_errors_without_generic_error_markers() {
        let summary = reduce_invocation(ReductionInput {
            events: &[],
            stdout: b"",
            stderr: b"mcp/js_fixture/type_mismatch.ts(6,3): error TS2322: Type 'string' is not assignable to type 'number'.\nERROR: Build did NOT complete successfully\n",
            exit_code: Some(1),
            elapsed_ms: 1,
            budget: Budget::result_default(),
        });

        let diagnostic = &summary.diagnostics[0];
        assert_eq!(
            diagnostic.message,
            "TS2322: Type 'string' is not assignable to type 'number'."
        );
        assert_eq!(diagnostic.category, DiagnosticCategory::Compilation);
        assert_eq!(
            diagnostic.location,
            Some(DiagnosticLocation {
                path: "mcp/js_fixture/type_mismatch.ts".into(),
                line: Some(6),
                column: Some(3),
            })
        );
        assert!(summary.headline.contains("TS2322"));
    }

    #[test]
    fn parses_typescript_pretty_diagnostics_and_javascript_inputs() {
        let diagnostic = parse_typescript_diagnostic(
            "/tmp/output/execroot/project/pkg/check.jsx:8:11 - warning TS6133: 'total' is declared but its value is never read.",
        )
        .unwrap();

        assert_eq!(diagnostic.severity, Severity::Warning);
        assert_eq!(
            diagnostic.message,
            "TS6133: 'total' is declared but its value is never read."
        );
        assert_eq!(diagnostic.location.unwrap().path, "pkg/check.jsx");
    }

    #[test]
    fn keeps_distinct_typescript_syntax_diagnostics() {
        let summary = reduce_invocation(ReductionInput {
            events: &[],
            stdout: b"",
            stderr: br#"mcp/js_fixture/syntax_failure.ts(8,8): error TS1005: ',' expected.
mcp/js_fixture/syntax_failure.ts(8,21): error TS1005: ',' expected.
mcp/js_fixture/syntax_failure.ts(9,1): error TS1005: '}' expected.
"#,
            exit_code: Some(1),
            elapsed_ms: 1,
            budget: Budget::result_default(),
        });

        assert_eq!(summary.diagnostics.len(), 3);
        assert_eq!(
            summary
                .diagnostics
                .iter()
                .filter_map(|diagnostic| diagnostic.location.as_ref()?.column)
                .collect::<Vec<_>>(),
            vec![8, 21, 1]
        );
    }

    #[test]
    fn structures_plain_swc_parser_failures_without_terminal_action_duplicates() {
        let event = BuildEvent {
            payload: Some(owned_build_event::Payload::Action(Box::new(
                ActionExecuted {
                    success: false,
                    exit_code: 1,
                    r#type: "SwcCompile".into(),
                    ..Default::default()
                },
            ))),
            ..Default::default()
        };
        let event = BepEvent::from_owned(&event).unwrap();
        let summary = reduce_invocation(ReductionInput {
            events: &[event],
            stdout: b"",
            stderr: include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/swc/parser-plain.stderr"
            )),
            exit_code: Some(1),
            elapsed_ms: 1,
            budget: Budget::result_default(),
        });

        let diagnostic = &summary.diagnostics[0];
        assert_eq!(diagnostic.category, DiagnosticCategory::Compilation);
        assert_eq!(
            diagnostic.message,
            "Expected ',', got 'identifier'\n3 | const total = invoice lines;\n:                       ^^^^^"
        );
        assert_eq!(
            diagnostic.location,
            Some(DiagnosticLocation {
                path: "pkg/parser.ts".into(),
                line: Some(3),
                column: Some(1),
            })
        );
        assert_eq!(
            summary
                .diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.category == DiagnosticCategory::Action)
                .count(),
            1
        );
        assert!(summary.diagnostics.iter().all(|diagnostic| {
            !diagnostic
                .message
                .contains("error executing SwcCompile command")
                && !diagnostic
                    .message
                    .contains("Build did NOT complete successfully")
        }));
    }

    #[test]
    fn normalizes_colorized_swc_target_diagnostics_and_runfiles_paths() {
        let summary = reduce_invocation(ReductionInput {
            events: &[],
            stdout: b"",
            stderr: include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/swc/target-color.stderr"
            )),
            exit_code: Some(1),
            elapsed_ms: 1,
            budget: Budget::result_default(),
        });

        assert_eq!(summary.diagnostics.len(), 1);
        let diagnostic = &summary.diagnostics[0];
        assert!(
            diagnostic
                .message
                .starts_with("Big integer literals are not available")
        );
        assert!(
            diagnostic
                .message
                .contains("SWC context: failed to transform module for target es5")
        );
        assert!(!diagnostic.message.contains('\u{1b}'));
        assert_eq!(
            diagnostic.location,
            Some(DiagnosticLocation {
                path: "pkg/target.js".into(),
                line: Some(1),
                column: Some(1),
            })
        );
    }

    #[test]
    fn captures_swc_transform_phase_context() {
        let summary = reduce_invocation(ReductionInput {
            events: &[],
            stdout: b"",
            stderr: include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/swc/transform-plain.stderr"
            )),
            exit_code: Some(1),
            elapsed_ms: 1,
            budget: Budget::result_default(),
        });

        assert_eq!(summary.diagnostics.len(), 1);
        assert!(
            summary.diagnostics[0]
                .message
                .contains("SWC context: failed to process JavaScript transform")
        );
        assert_eq!(
            summary.diagnostics[0].location.as_ref().unwrap().path,
            "pkg/transform.mjs"
        );
    }

    #[test]
    fn captures_colorized_swc_plugin_failures_and_plugin_context() {
        let summary = reduce_invocation(ReductionInput {
            events: &[],
            stdout: b"",
            stderr: include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/swc/plugin-color.stderr"
            )),
            exit_code: Some(1),
            elapsed_ms: 1,
            budget: Budget::result_default(),
        });

        assert_eq!(summary.diagnostics.len(), 1);
        let diagnostic = &summary.diagnostics[0];
        assert!(diagnostic.message.contains("failed to invoke plugin"));
        assert!(diagnostic.message.contains("@swc/plugin-styled-components"));
        assert!(diagnostic.message.contains("5 │ export const Price"));
        assert_eq!(
            diagnostic.location.as_ref().unwrap().path,
            "pkg/component.tsx"
        );
    }

    #[test]
    fn bounds_swc_source_frames() {
        let source = "x".repeat(2_000);
        let input = format!(
            "  x Expression expected\n   ,-[/tmp/output/execroot/_main/pkg/large.ts:1:1]\n 1 | {source}\n   : ^\n   `----"
        );
        let parsed = parse_swc_diagnostics(&input);

        assert_eq!(parsed.diagnostics.len(), 1);
        assert!(parsed.diagnostics[0].message.len() < 600);
        assert!(parsed.diagnostics[0].message.contains('…'));
    }

    #[test]
    fn selects_the_located_line_from_multiline_swc_source_frames() {
        let input = r#"  x Expression expected
   ,-[cases/swc_syntax_failure.ts:2:1]
 1 | export const invoice = {
 2 |   total: ,
   :          ^
 3 | };
   `----
Caused by:
    0: failed to process js file
    1: Syntax Error"#;
        let parsed = parse_swc_diagnostics(input);

        assert_eq!(parsed.diagnostics.len(), 1);
        assert!(parsed.diagnostics[0].message.contains("2 |   total: ,"));
        assert!(!parsed.diagnostics[0].message.contains("1 | export"));
        assert_eq!(
            parsed.diagnostics[0].location,
            Some(DiagnosticLocation {
                path: "cases/swc_syntax_failure.ts".into(),
                line: Some(2),
                column: Some(1),
            })
        );
    }

    #[test]
    fn extracts_node_exceptions_from_application_stack_frames() {
        let summary = reduce_invocation(ReductionInput {
            events: &[],
            stdout: b"",
            stderr: br#"/tmp/output/runtime_type_error.runfiles/_main/mcp/js_fixture/runtime_type_error.js:2
return invoice.lines.reduce((total, line) => total + line.amount, 0);
               ^

TypeError: Cannot read properties of undefined (reading 'lines')
    at calculateInvoiceTotal (/tmp/output/runtime_type_error.runfiles/_main/mcp/js_fixture/runtime_type_error.js:2:18)
    at Object.<anonymous> (/tmp/output/runtime_type_error.runfiles/_main/mcp/js_fixture/runtime_type_error.js:6:1)
"#,
            exit_code: Some(1),
            elapsed_ms: 1,
            budget: Budget::result_default(),
        });

        assert_eq!(summary.diagnostics.len(), 1);
        let diagnostic = &summary.diagnostics[0];
        assert_eq!(diagnostic.category, DiagnosticCategory::Test);
        assert_eq!(
            diagnostic.message,
            "TypeError: Cannot read properties of undefined (reading 'lines')"
        );
        assert_eq!(
            diagnostic.location,
            Some(DiagnosticLocation {
                path: "mcp/js_fixture/runtime_type_error.js".into(),
                line: Some(2),
                column: Some(18),
            })
        );
    }

    #[test]
    fn pairs_node_syntax_errors_with_the_source_header() {
        let summary = reduce_invocation(ReductionInput {
            events: &[],
            stdout: b"",
            stderr: br#"/tmp/output/runtime_syntax_error.runfiles/_main/mcp/js_fixture/runtime_syntax_error.js:4
console.log(invoice);
       ^

SyntaxError: Unexpected token '.'
    at wrapSafe (node:internal/modules/cjs/loader:1638:18)
    at Module._compile (node:internal/modules/cjs/loader:1680:20)
"#,
            exit_code: Some(1),
            elapsed_ms: 1,
            budget: Budget::result_default(),
        });

        assert_eq!(summary.diagnostics.len(), 1);
        assert_eq!(
            summary.diagnostics[0].message,
            "SyntaxError: Unexpected token '.'"
        );
        assert_eq!(
            summary.diagnostics[0].location,
            Some(DiagnosticLocation {
                path: "mcp/js_fixture/runtime_syntax_error.js".into(),
                line: Some(4),
                column: None,
            })
        );
    }

    #[test]
    fn structures_protobuf_syntax_errors_without_error_markers() {
        let summary = reduce_invocation(ReductionInput {
            events: &[],
            stdout: b"",
            stderr: br#"mcp/proto_fixture/syntax_failure.proto:7:1: Expected ";".
ERROR: Build did NOT complete successfully
"#,
            exit_code: Some(1),
            elapsed_ms: 1,
            budget: Budget::result_default(),
        });

        let diagnostic = &summary.diagnostics[0];
        assert_eq!(diagnostic.message, "Expected \";\".");
        assert_eq!(diagnostic.category, DiagnosticCategory::Compilation);
        assert_eq!(
            diagnostic.location,
            Some(DiagnosticLocation {
                path: "mcp/proto_fixture/syntax_failure.proto".into(),
                line: Some(7),
                column: Some(1),
            })
        );
        assert!(summary.headline.contains("Expected"));
    }

    #[test]
    fn ranks_the_located_protobuf_import_failure_ahead_of_missing_file_noise() {
        let summary = reduce_invocation(ReductionInput {
            events: &[],
            stdout: b"",
            stderr: br#"mcp/proto_fixture/does_not_exist.proto: File not found.
mcp/proto_fixture/missing_import.proto:5:1: Import "mcp/proto_fixture/does_not_exist.proto" was not found or had errors.
mcp/proto_fixture/missing_import.proto:8:3: "MissingDependency" is not defined.
"#,
            exit_code: Some(1),
            elapsed_ms: 1,
            budget: Budget::result_default(),
        });

        assert_eq!(summary.diagnostics.len(), 2);
        assert_eq!(
            summary.diagnostics[0].message,
            "Import \"mcp/proto_fixture/does_not_exist.proto\" was not found or had errors."
        );
        assert_eq!(
            summary.diagnostics[0].location.as_ref().unwrap().line,
            Some(5)
        );
    }

    #[test]
    fn parses_protobuf_warnings_and_compacts_execroot_paths() {
        let diagnostic = parse_protobuf_diagnostic(
            "/tmp/output/execroot/project/pkg/schema.proto:4:2: warning: Import common.proto is unused.",
        )
        .unwrap();

        assert_eq!(diagnostic.severity, Severity::Warning);
        assert_eq!(diagnostic.message, "Import common.proto is unused.");
        assert_eq!(diagnostic.location.unwrap().path, "pkg/schema.proto");
    }

    #[test]
    fn structures_java_compiler_errors_and_retains_symbol_details() {
        let summary = reduce_invocation(ReductionInput {
            events: &[],
            stdout: b"",
            stderr: br#"mcp/java_fixture/MissingSymbol.java:5: error: cannot find symbol
    MissingInvoiceCalculator calculator = new MissingInvoiceCalculator();
    ^
  symbol:   class MissingInvoiceCalculator
  location: class MissingSymbol
ERROR: Building mcp/java_fixture/libmissing_symbol.jar failed: error executing Javac command
"#,
            exit_code: Some(1),
            elapsed_ms: 1,
            budget: Budget::result_default(),
        });

        let diagnostic = &summary.diagnostics[0];
        assert_eq!(
            diagnostic.message,
            "cannot find symbol: class MissingInvoiceCalculator"
        );
        assert_eq!(diagnostic.category, DiagnosticCategory::Compilation);
        assert_eq!(
            diagnostic.location,
            Some(DiagnosticLocation {
                path: "mcp/java_fixture/MissingSymbol.java".into(),
                line: Some(5),
                column: None,
            })
        );
    }

    #[test]
    fn structures_java_type_errors_without_context_lines() {
        let summary = reduce_invocation(ReductionInput {
            events: &[],
            stdout: b"",
            stderr: b"mcp/java_fixture/TypeMismatch.java:5: error: incompatible types: String cannot be converted to int\n",
            exit_code: Some(1),
            elapsed_ms: 1,
            budget: Budget::result_default(),
        });

        assert_eq!(
            summary.diagnostics[0].message,
            "incompatible types: String cannot be converted to int"
        );
        assert_eq!(
            summary.diagnostics[0].location.as_ref().unwrap().line,
            Some(5)
        );
    }

    #[test]
    fn extracts_java_test_failures_from_the_first_application_frame() {
        let summary = reduce_invocation(ReductionInput {
            events: &[],
            stdout: b"",
            stderr: br#"java.lang.AssertionError: invoice total mismatch
    at org.junit.Assert.fail(Assert.java:89)
    at org.junit.Assert.assertEquals(Assert.java:120)
    at mcp.java_fixture.RuntimeFailure.assertInvoiceTotal(RuntimeFailure.java:9)
    at mcp.java_fixture.RuntimeFailure.main(RuntimeFailure.java:5)
"#,
            exit_code: Some(1),
            elapsed_ms: 1,
            budget: Budget::result_default(),
        });

        let diagnostic = &summary.diagnostics[0];
        assert_eq!(
            diagnostic.message,
            "java.lang.AssertionError: invoice total mismatch"
        );
        assert_eq!(diagnostic.category, DiagnosticCategory::Test);
        assert_eq!(
            diagnostic.location,
            Some(DiagnosticLocation {
                path: "mcp/java_fixture/RuntimeFailure.java".into(),
                line: Some(9),
                column: None,
            })
        );
    }

    #[test]
    fn ranks_starlark_syntax_errors_with_structured_locations() {
        let summary = reduce_invocation(ReductionInput {
            events: &[],
            stdout: b"",
            stderr: br#"ERROR: /tmp/project/pkg/BUILD.bazel:5:1: syntax error at ')': expected ]
ERROR: no such target '//pkg:broken': target 'broken' not declared in package 'pkg'
ERROR: Build did NOT complete successfully
"#,
            exit_code: Some(1),
            elapsed_ms: 1,
            budget: Budget::result_default(),
        });

        let diagnostic = &summary.diagnostics[0];
        assert_eq!(diagnostic.message, "syntax error at ')': expected ]");
        assert_eq!(diagnostic.category, DiagnosticCategory::Loading);
        assert_eq!(
            diagnostic.location,
            Some(DiagnosticLocation {
                path: "/tmp/project/pkg/BUILD.bazel".into(),
                line: Some(5),
                column: Some(1),
            })
        );
        assert!(summary.headline.contains("syntax error"));
    }

    #[test]
    fn compacts_sanitized_workspace_markers_in_source_locations() {
        let diagnostic = parse_starlark_inline_diagnostic(
            "ERROR: <WORKSPACE>/pkg/defs.bzl:4:2: syntax error at ')': expected ]",
        )
        .unwrap();

        assert_eq!(diagnostic.location.unwrap().path, "pkg/defs.bzl");
    }

    #[test]
    fn extracts_starlark_macro_failure_from_the_innermost_frame() {
        let summary = reduce_invocation(ReductionInput {
            events: &[],
            stdout: b"",
            stderr: br#"ERROR: Traceback (most recent call last):
    File "/tmp/project/pkg/BUILD.bazel", line 3, column 20, in <toplevel>
        validated_filegroup(
    File "/tmp/project/pkg/defs.bzl", line 3, column 13, in validated_filegroup
        fail("production targets must declare an owner")
Error in fail: production targets must declare an owner
ERROR: no such target '//pkg:broken': target 'broken' not declared in package 'pkg'
"#,
            exit_code: Some(1),
            elapsed_ms: 1,
            budget: Budget::result_default(),
        });

        let diagnostic = &summary.diagnostics[0];
        assert_eq!(
            diagnostic.message,
            "Error in fail: production targets must declare an owner"
        );
        assert_eq!(diagnostic.category, DiagnosticCategory::Loading);
        assert_eq!(
            diagnostic.location,
            Some(DiagnosticLocation {
                path: "/tmp/project/pkg/defs.bzl".into(),
                line: Some(3),
                column: Some(13),
            })
        );
    }

    #[test]
    fn classifies_rule_implementation_tracebacks_as_analysis_failures() {
        let summary = reduce_invocation(ReductionInput {
            events: &[],
            stdout: b"",
            stderr:
                br#"ERROR: /tmp/project/pkg/BUILD.bazel:3:17: in validated_target rule //pkg:broken:
Traceback (most recent call last):
    File "/tmp/project/pkg/rule.bzl", line 3, column 13, in _validated_target_impl
        fail("production target requires a release ticket")
Error in fail: production target requires a release ticket
ERROR: /tmp/project/pkg/BUILD.bazel:3:17: Analysis of target '//pkg:broken' failed
"#,
            exit_code: Some(1),
            elapsed_ms: 1,
            budget: Budget::result_default(),
        });

        let diagnostic = &summary.diagnostics[0];
        assert_eq!(
            diagnostic.message,
            "Error in fail: production target requires a release ticket"
        );
        assert_eq!(diagnostic.category, DiagnosticCategory::Analysis);
        assert_eq!(
            diagnostic.location,
            Some(DiagnosticLocation {
                path: "/tmp/project/pkg/rule.bzl".into(),
                line: Some(3),
                column: Some(13),
            })
        );
    }

    #[test]
    fn ranks_python_syntax_errors_ahead_of_pycompile_wrappers() {
        let stderr = br#"Unhandled error:
Traceback (most recent call last):
  File "/opt/python/lib/python3.11/py_compile.py", line 144, in compile
  File "mcp_reducer_fixture/syntax_failure_test.py", line 6
    configuration = {
                    ^
SyntaxError: '{' was never closed
py_compile.PyCompileError:   File "mcp_reducer_fixture/syntax_failure_test.py", line 6
SyntaxError: '{' was never closed
ERROR: Build did NOT complete successfully
"#;
        let summary = reduce_invocation(ReductionInput {
            events: &[],
            stdout: b"",
            stderr,
            exit_code: Some(1),
            elapsed_ms: 1,
            budget: Budget::result_default(),
        });

        let diagnostic = &summary.diagnostics[0];
        assert_eq!(diagnostic.message, "SyntaxError: '{' was never closed");
        assert_eq!(diagnostic.category, DiagnosticCategory::Compilation);
        assert_eq!(diagnostic.repetition_count, 2);
        assert_eq!(
            diagnostic.location,
            Some(DiagnosticLocation {
                path: "mcp_reducer_fixture/syntax_failure_test.py".into(),
                line: Some(6),
                column: None,
            })
        );
        assert!(summary.headline.contains("SyntaxError"));
    }

    #[test]
    fn extracts_python_test_traceback_locations_from_bazel_runfiles() {
        let stderr = br#"Traceback (most recent call last):
  File "/tmp/output/test.runfiles/_main/pkg/_pricing_test_stage2_bootstrap.py", line 588, in <module>
  File "/tmp/output/test.runfiles/_main/pkg/pricing_test.py", line 7, in test_total
    self.assertEqual(actual_total, 41)
AssertionError: 42 != 41
"#;
        let summary = reduce_invocation(ReductionInput {
            events: &[],
            stdout: b"",
            stderr,
            exit_code: Some(1),
            elapsed_ms: 1,
            budget: Budget::result_default(),
        });

        assert_eq!(summary.diagnostics.len(), 1);
        assert_eq!(summary.diagnostics[0].message, "AssertionError: 42 != 41");
        assert_eq!(
            summary.diagnostics[0].location,
            Some(DiagnosticLocation {
                path: "pkg/pricing_test.py".into(),
                line: Some(7),
                column: None,
            })
        );
    }

    #[test]
    fn preserves_locations_after_recording_path_sanitization() {
        let mut parser = PythonDiagnosticParser::default();
        parser.observe_line(
            "  File \"<RUNTIME_ROOT>/sandbox/execroot/_main/bazel-out/bin/test.runfiles/_main/pkg/test_invoice.py\", line 7",
        );
        let diagnostic = parser
            .observe_line("ValueError: unsupported currency")
            .unwrap();

        assert_eq!(diagnostic.location.unwrap().path, "pkg/test_invoice.py");
    }

    #[test]
    fn recognizes_pytest_exception_prefixes() {
        let mut parser = PythonDiagnosticParser::default();
        assert!(
            parser
                .observe_line("  File \"pkg/test_checkout.py\", line 19, in test_total")
                .is_none()
        );
        let diagnostic = parser
            .observe_line("E       ValueError: invalid discount")
            .unwrap();

        assert_eq!(diagnostic.message, "ValueError: invalid discount");
        assert_eq!(diagnostic.location.unwrap().line, Some(19));
    }

    #[test]
    fn keeps_identical_python_messages_at_distinct_locations() {
        let summary = reduce_invocation(ReductionInput {
            events: &[],
            stdout: b"",
            stderr: br#"  File "pkg/first_test.py", line 3, in test_value
AssertionError: mismatch
  File "pkg/second_test.py", line 8, in test_value
AssertionError: mismatch
"#,
            exit_code: Some(1),
            elapsed_ms: 1,
            budget: Budget::result_default(),
        });

        assert_eq!(summary.diagnostics.len(), 2);
        assert_eq!(
            summary
                .diagnostics
                .iter()
                .filter_map(|diagnostic| diagnostic.location.as_ref())
                .map(|location| location.path.as_str())
                .collect::<Vec<_>>(),
            vec!["pkg/first_test.py", "pkg/second_test.py"]
        );
    }

    #[test]
    fn keeps_failed_rust_evidence_and_rejects_successful_test_names() {
        let stderr = b"test build::tests::successful_root_cause_test ... ok\n\
test test::tests::fails ... FAILED\n\
failures:\n\
thread 'test::tests::fails' panicked at src/test.rs:7:9:\n\
assertion `left == right` failed\n";
        let summary = reduce_invocation(ReductionInput {
            events: &[],
            stdout: b"",
            stderr,
            exit_code: Some(1),
            elapsed_ms: 1,
            budget: Budget::result_default(),
        });

        assert!(
            summary
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("panicked at"))
        );
        assert!(
            summary
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("assertion"))
        );
        assert!(summary.diagnostics.iter().all(|diagnostic| {
            !diagnostic.message.contains("successful_root_cause_test")
                && diagnostic.message != "failures:"
        }));
    }

    #[test]
    fn ranks_combined_rust_failure_above_single_log_lines() {
        let diagnostic = |message: &str, location| Diagnostic {
            severity: Severity::Error,
            category: DiagnosticCategory::Test,
            message: message.to_owned(),
            location,
            target: Some("//pkg:test".to_owned()),
            action: None,
            repetition_count: 1,
        };
        let mut summary = InvocationSummary {
            success: false,
            diagnostics: vec![
                diagnostic("thread 'tests::fails' panicked at src/test.rs:7:9:", None),
                diagnostic("assertion `left == right` failed", None),
                diagnostic(
                    "Rust test tests::fails failed at src/test.rs:7:9: assertion `left == right` failed; left: 1; right: 2",
                    Some(bazel_mcp_types::DiagnosticLocation {
                        path: "src/test.rs".to_owned(),
                        line: Some(7),
                        column: Some(9),
                    }),
                ),
            ],
            ..InvocationSummary::default()
        };

        finalize_diagnostics(&mut summary, Budget::result_default());

        assert!(summary.diagnostics[0].message.starts_with("Rust test"));
        assert!(summary.headline.contains("left: 1; right: 2"));
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
