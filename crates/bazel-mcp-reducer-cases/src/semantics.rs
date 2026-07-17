use std::fmt;

use bazel_mcp_types::{Artifact, Diagnostic};
use regex::Regex;

use crate::{ArtifactExpectation, CaseExpectation, DiagnosticExpectation, ReplayOutput};

#[derive(Clone, Debug)]
pub struct CaseObservation {
    pub state: String,
    pub exit_code: Option<i32>,
    pub headline: String,
    pub inspect_hint: Option<String>,
    pub diagnostics: Vec<Diagnostic>,
    pub artifacts: Vec<Artifact>,
    pub visible_bytes: usize,
    pub raw_text: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerificationFailure {
    pub messages: Vec<String>,
}

impl fmt::Display for VerificationFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.messages.join("\n"))
    }
}

impl std::error::Error for VerificationFailure {}

pub fn observe_replay(output: &ReplayOutput, exit_code: i32, raw_text: String) -> CaseObservation {
    let visible_bytes =
        serde_json::to_vec(&output.summary).map_or(usize::MAX, |encoded| encoded.len());
    CaseObservation {
        state: if output.summary.success {
            "succeeded".to_owned()
        } else {
            "failed".to_owned()
        },
        exit_code: Some(exit_code),
        headline: output.summary.headline.clone(),
        inspect_hint: output
            .summary
            .inspect_hint
            .map(|hint| hint.as_str().to_owned()),
        diagnostics: output.summary.diagnostics.clone(),
        artifacts: output.artifacts.clone(),
        visible_bytes,
        raw_text,
    }
}

pub fn verify_expectations(
    case_id: &str,
    expected: &CaseExpectation,
    actual: &CaseObservation,
) -> Result<(), VerificationFailure> {
    let mut failures = Vec::new();
    if actual.state != expected.state {
        failures.push(format!(
            "{case_id}: expected state {:?}, observed {:?}",
            expected.state, actual.state
        ));
    }
    if let Some(exit_code) = expected.exit_code
        && actual.exit_code != Some(exit_code)
    {
        failures.push(format!(
            "{case_id}: expected exit code {exit_code}, observed {:?}",
            actual.exit_code
        ));
    }
    if let Some(headline) = &expected.headline_equals
        && actual.headline != *headline
    {
        failures.push(format!(
            "{case_id}: expected headline {headline:?}, observed {:?}",
            actual.headline
        ));
    }
    if let Some(fragment) = &expected.headline_contains
        && !actual.headline.contains(fragment)
    {
        failures.push(format!(
            "{case_id}: headline does not contain {fragment:?}: {:?}",
            actual.headline
        ));
    }
    if let Some(inspect_hint) = &expected.inspect_hint
        && actual.inspect_hint.as_deref() != Some(inspect_hint)
    {
        failures.push(format!(
            "{case_id}: expected inspect hint {inspect_hint:?}, observed {:?}",
            actual.inspect_hint
        ));
    }
    if actual.visible_bytes > expected.max_visible_bytes {
        failures.push(format!(
            "{case_id}: visible output is {} bytes, over the {} byte contract",
            actual.visible_bytes, expected.max_visible_bytes
        ));
    }
    if actual.diagnostics.len() > expected.max_diagnostics {
        failures.push(format!(
            "{case_id}: {} diagnostics exceed the {} item contract",
            actual.diagnostics.len(),
            expected.max_diagnostics
        ));
    }
    for diagnostic in &expected.diagnostics {
        verify_diagnostic(case_id, diagnostic, &actual.diagnostics, &mut failures);
    }
    for artifact in &expected.artifacts {
        if !actual
            .artifacts
            .iter()
            .any(|candidate| artifact_matches(candidate, artifact))
        {
            failures.push(format!(
                "{case_id}: no artifact matched expectation {artifact:?}"
            ));
        }
    }
    for forbidden in &expected.absent.message_contains {
        if actual
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains(forbidden))
        {
            failures.push(format!(
                "{case_id}: a diagnostic contains forbidden text {forbidden:?}"
            ));
        }
    }
    for forbidden in &expected.absent.path_contains {
        if actual.diagnostics.iter().any(|diagnostic| {
            diagnostic
                .location
                .as_ref()
                .is_some_and(|location| location.path.contains(forbidden))
        }) {
            failures.push(format!(
                "{case_id}: a diagnostic path contains forbidden text {forbidden:?}"
            ));
        }
    }
    for forbidden in &expected.absent.target_contains {
        if actual.diagnostics.iter().any(|diagnostic| {
            diagnostic
                .target
                .as_ref()
                .is_some_and(|target| target.contains(forbidden))
        }) {
            failures.push(format!(
                "{case_id}: a diagnostic target contains forbidden text {forbidden:?}"
            ));
        }
    }
    for forbidden in &expected.absent.artifact_uri_contains {
        if actual
            .artifacts
            .iter()
            .any(|artifact| artifact.uri.contains(forbidden))
        {
            failures.push(format!(
                "{case_id}: an artifact URI contains forbidden text {forbidden:?}"
            ));
        }
    }
    for forbidden in &expected.absent.raw_contains {
        if actual.raw_text.contains(forbidden) {
            failures.push(format!(
                "{case_id}: retained raw text contains forbidden text {forbidden:?}"
            ));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(VerificationFailure { messages: failures })
    }
}

/// Checks the stable, user-facing fields that a live MCP invocation and its
/// sanitized replay must agree on. Volatile timing, artifact URIs, and lower
/// ranked wrapper diagnostics are intentionally excluded.
pub fn verify_live_replay_parity(
    case_id: &str,
    live: &CaseObservation,
    replay: &CaseObservation,
) -> Result<(), VerificationFailure> {
    let mut failures = Vec::new();
    if live.state != replay.state {
        failures.push(format!(
            "{case_id}: live state {:?} differs from replay {:?}",
            live.state, replay.state
        ));
    }
    if live.exit_code != replay.exit_code {
        failures.push(format!(
            "{case_id}: live exit code {:?} differs from replay {:?}",
            live.exit_code, replay.exit_code
        ));
    }
    if normalize_placeholders(&live.headline) != normalize_placeholders(&replay.headline) {
        failures.push(format!(
            "{case_id}: live headline {:?} differs from replay {:?}",
            live.headline, replay.headline
        ));
    }
    let hints_match = live.inspect_hint == replay.inspect_hint
        || (live.inspect_hint.as_deref() == Some("log") && replay.inspect_hint.is_none());
    if !hints_match {
        failures.push(format!(
            "{case_id}: live inspect hint {:?} differs from replay {:?}",
            live.inspect_hint, replay.inspect_hint
        ));
    }
    match (live.diagnostics.first(), replay.diagnostics.first()) {
        (Some(live), Some(replay))
            if live.severity != replay.severity
                || !diagnostic_classification_matches(live, replay)
                || normalize_placeholders(&live.message)
                    != normalize_placeholders(&replay.message)
                || live.location != replay.location
                || live.action != replay.action
                || live.repetition_count != replay.repetition_count =>
        {
            failures.push(format!(
                "{case_id}: live primary diagnostic {live:?} differs from replay {replay:?}"
            ));
        }
        (Some(_), None) | (None, Some(_)) => failures.push(format!(
            "{case_id}: live and replay disagree on whether a primary diagnostic exists"
        )),
        _ => {}
    }
    let live_artifacts = live
        .artifacts
        .iter()
        .map(|artifact| (&artifact.name, &artifact.kind))
        .collect::<Vec<_>>();
    let replay_artifacts = replay
        .artifacts
        .iter()
        .map(|artifact| (&artifact.name, &artifact.kind))
        .collect::<Vec<_>>();
    if live_artifacts != replay_artifacts {
        failures.push(format!(
            "{case_id}: live artifact names/kinds {live_artifacts:?} differ from replay {replay_artifacts:?}"
        ));
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(VerificationFailure { messages: failures })
    }
}

fn normalize_placeholders(value: &str) -> String {
    Regex::new(r"<(?:WORKSPACE|workspace)>_*")
        .expect("valid workspace placeholder regex")
        .replace_all(value, "<workspace>")
        .into_owned()
}

fn diagnostic_classification_matches(live: &Diagnostic, replay: &Diagnostic) -> bool {
    if live.category == replay.category && live.target == replay.target {
        return true;
    }
    // Failed test output may be parsed once from --test_output=errors before
    // the service snapshots the same line from test.log. The replay always has
    // the snapshot, so treat that promotion as equivalent while the manifest
    // remains free to require the stronger Test classification.
    matches!(
        (&live.category, &replay.category),
        (
            bazel_mcp_types::DiagnosticCategory::Compilation,
            bazel_mcp_types::DiagnosticCategory::Test
        ) | (
            bazel_mcp_types::DiagnosticCategory::Test,
            bazel_mcp_types::DiagnosticCategory::Compilation
        )
    ) && (live.target.is_none() || replay.target.is_none())
}

fn verify_diagnostic(
    case_id: &str,
    expected: &DiagnosticExpectation,
    diagnostics: &[Diagnostic],
    failures: &mut Vec<String>,
) {
    if let Some(rank) = expected.rank {
        let Some(actual) = diagnostics.get(rank) else {
            failures.push(format!(
                "{case_id}: expected diagnostic rank {rank}, only {} diagnostics exist",
                diagnostics.len()
            ));
            return;
        };
        if !diagnostic_matches(actual, expected) {
            failures.push(format!(
                "{case_id}: diagnostic at rank {rank} did not match {expected:?}; observed {actual:?}"
            ));
        }
    } else if !diagnostics
        .iter()
        .any(|diagnostic| diagnostic_matches(diagnostic, expected))
    {
        failures.push(format!(
            "{case_id}: no diagnostic matched expectation {expected:?}"
        ));
    }
}

fn diagnostic_matches(actual: &Diagnostic, expected: &DiagnosticExpectation) -> bool {
    expected
        .severity
        .as_deref()
        .is_none_or(|value| serde_name(&actual.severity) == value)
        && expected
            .category
            .as_deref()
            .is_none_or(|value| serde_name(&actual.category) == value)
        && expected
            .message_equals
            .as_deref()
            .is_none_or(|value| actual.message == value)
        && expected
            .message_prefix
            .as_deref()
            .is_none_or(|value| actual.message.starts_with(value))
        && expected
            .message_contains
            .as_deref()
            .is_none_or(|value| actual.message.contains(value))
        && expected.path.as_deref().is_none_or(|value| {
            actual
                .location
                .as_ref()
                .is_some_and(|location| location.path == value)
        })
        && expected.line.is_none_or(|value| {
            actual
                .location
                .as_ref()
                .is_some_and(|location| location.line == Some(value))
        })
        && expected.column.is_none_or(|value| {
            actual
                .location
                .as_ref()
                .is_some_and(|location| location.column == Some(value))
        })
        && expected
            .target
            .as_deref()
            .is_none_or(|value| actual.target.as_deref() == Some(value))
        && expected
            .action
            .as_deref()
            .is_none_or(|value| actual.action.as_deref() == Some(value))
        && expected
            .repetition_count
            .is_none_or(|value| actual.repetition_count == value)
}

fn artifact_matches(actual: &Artifact, expected: &ArtifactExpectation) -> bool {
    expected
        .name_equals
        .as_deref()
        .is_none_or(|value| actual.name == value)
        && expected
            .name_contains
            .as_deref()
            .is_none_or(|value| actual.name.contains(value))
        && expected
            .kind
            .as_deref()
            .is_none_or(|value| serde_name(&actual.kind) == value)
        && expected
            .uri_contains
            .as_deref()
            .is_none_or(|value| actual.uri.contains(value))
        && expected
            .locally_available
            .is_none_or(|value| actual.locally_available == value)
}

fn serde_name<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use bazel_mcp_types::{DiagnosticCategory, DiagnosticLocation, Severity};

    use super::*;
    use crate::{AbsentExpectation, CaseExpectation, DiagnosticExpectation};

    fn expected() -> CaseExpectation {
        CaseExpectation {
            state: "failed".to_owned(),
            exit_code: Some(1),
            headline_equals: None,
            headline_contains: Some("missing symbol".to_owned()),
            inspect_hint: None,
            max_visible_bytes: 8192,
            max_diagnostics: 20,
            diagnostics: vec![DiagnosticExpectation {
                rank: Some(0),
                severity: Some("error".to_owned()),
                category: Some("compilation".to_owned()),
                message_equals: None,
                message_prefix: None,
                message_contains: Some("missing symbol".to_owned()),
                path: Some("src/main.cc".to_owned()),
                line: Some(7),
                column: None,
                target: None,
                action: None,
                repetition_count: None,
            }],
            artifacts: Vec::new(),
            absent: AbsentExpectation {
                message_contains: vec!["linker command failed".to_owned()],
                ..AbsentExpectation::default()
            },
        }
    }

    fn observation() -> CaseObservation {
        CaseObservation {
            state: "failed".to_owned(),
            exit_code: Some(1),
            headline: "Bazel failed: missing symbol invoice_total".to_owned(),
            inspect_hint: None,
            diagnostics: vec![Diagnostic {
                severity: Severity::Error,
                category: DiagnosticCategory::Compilation,
                message: "missing symbol invoice_total".to_owned(),
                location: Some(DiagnosticLocation {
                    path: "src/main.cc".to_owned(),
                    line: Some(7),
                    column: None,
                }),
                target: None,
                action: None,
                repetition_count: 1,
            }],
            artifacts: Vec::new(),
            visible_bytes: 200,
            raw_text: String::new(),
        }
    }

    #[test]
    fn verifies_structured_and_negative_expectations() {
        verify_expectations("cpp/link", &expected(), &observation()).unwrap();
    }

    #[test]
    fn reports_all_semantic_mismatches() {
        let mut actual = observation();
        actual.state = "succeeded".to_owned();
        actual.diagnostics[0]
            .message
            .push_str(" linker command failed");
        let error = verify_expectations("cpp/link", &expected(), &actual).unwrap_err();
        assert!(error.messages.len() >= 2);
    }

    #[test]
    fn live_replay_parity_ignores_volatile_fields() {
        let live = observation();
        let mut replay = live.clone();
        replay.visible_bytes = 999;
        replay.raw_text = "different retained evidence".to_owned();
        verify_live_replay_parity("cpp/link", &live, &replay).unwrap();
    }
}
