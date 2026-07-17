//! Streaming, deterministic reduction of normalized failed-test evidence.

use bazel_mcp_types::{Diagnostic, DiagnosticCategory, Severity, TestCase, TestStatus};

use crate::{
    JavaScriptTestDiagnosticParser, JavaTestDiagnosticParser, PythonDiagnosticParser,
    TestFailureAccumulator, TestFailureEvidence, parse_go_diagnostic,
};

const MAX_FAILURES: usize = 20;
const MAX_MESSAGE_BYTES: usize = 1_000;

/// Stable context for one failed Bazel test target's evidence stream.
pub struct TestEvidenceInput<'a> {
    pub label: &'a str,
}

/// Deterministically reduced evidence for one failed Bazel test target.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TestEvidenceResult {
    pub cases: Vec<TestCase>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Streaming reducer for language-specific and framework-specific test output.
///
/// Acquisition stays in the runner so raw logs can be durably retained before
/// this reducer sees normalized, redacted lines.
pub struct TestEvidenceReducer {
    label: String,
    javascript: JavaScriptTestDiagnosticParser,
    java: JavaTestDiagnosticParser,
    python: PythonDiagnosticParser,
    failures: TestFailureAccumulator,
    extracted_failures: Vec<TestFailureEvidence>,
    fallback: Option<(u8, Diagnostic)>,
}

impl TestEvidenceReducer {
    #[must_use]
    pub fn new(input: TestEvidenceInput<'_>) -> Self {
        Self {
            label: input.label.to_owned(),
            javascript: JavaScriptTestDiagnosticParser::default(),
            java: JavaTestDiagnosticParser::default(),
            python: PythonDiagnosticParser::default(),
            failures: TestFailureAccumulator::default(),
            extracted_failures: Vec::new(),
            fallback: None,
        }
    }

    pub fn observe_line(&mut self, line: &str) {
        let label = &self.label;
        self.failures.observe_line(line);
        let javascript_diagnostic = self.javascript.observe_line(line);
        let java_diagnostic = self.java.observe_line(line);
        let candidate = if let Some(mut diagnostic) = parse_go_diagnostic(line) {
            diagnostic.category = DiagnosticCategory::Test;
            diagnostic.target = Some(label.to_owned());
            diagnostic.message = bounded_text(&diagnostic.message, MAX_MESSAGE_BYTES);
            Some((0, diagnostic))
        } else if let Some(mut diagnostic) = javascript_diagnostic {
            diagnostic.target = Some(label.to_owned());
            diagnostic.message = bounded_text(&diagnostic.message, MAX_MESSAGE_BYTES);
            Some((0, diagnostic))
        } else if let Some(mut diagnostic) = java_diagnostic {
            diagnostic.target = Some(label.to_owned());
            diagnostic.message = bounded_text(&diagnostic.message, MAX_MESSAGE_BYTES);
            Some((0, diagnostic))
        } else if let Some(mut diagnostic) = self.python.observe_line(line) {
            diagnostic.category = DiagnosticCategory::Test;
            diagnostic.target = Some(label.to_owned());
            diagnostic.message = bounded_text(&diagnostic.message, MAX_MESSAGE_BYTES);
            Some((0, diagnostic))
        } else {
            failure_evidence_priority(line).map(|priority| {
                (
                    priority,
                    Diagnostic {
                        severity: Severity::Error,
                        category: DiagnosticCategory::Test,
                        message: bounded_text(line, MAX_MESSAGE_BYTES),
                        location: None,
                        target: Some(label.to_owned()),
                        action: None,
                        repetition_count: 1,
                    },
                )
            })
        };
        if let Some((priority, diagnostic)) = candidate
            && self
                .fallback
                .as_ref()
                .is_none_or(|(current, current_diagnostic)| {
                    priority < *current
                        || (priority == *current
                            && diagnostic.location.is_some()
                            && current_diagnostic.location.is_none())
                })
        {
            self.fallback = Some((priority, diagnostic));
        }
    }

    /// Completes the current log. Incomplete logs do not contribute structured
    /// failures, matching the runner's durable snapshot semantics.
    pub fn finish_log(&mut self, complete: bool) {
        if complete {
            for mut diagnostic in [self.javascript.finish(), self.java.finish()]
                .into_iter()
                .flatten()
            {
                diagnostic.target = Some(self.label.clone());
                diagnostic.message = bounded_text(&diagnostic.message, MAX_MESSAGE_BYTES);
                if self.fallback.as_ref().is_none_or(|(priority, current)| {
                    *priority > 0
                        || (*priority == 0
                            && diagnostic.location.is_some()
                            && current.location.is_none())
                }) {
                    self.fallback = Some((0, diagnostic));
                }
            }
            for failure in std::mem::take(&mut self.failures).finish() {
                if self.extracted_failures.len() >= MAX_FAILURES {
                    break;
                }
                if !self.extracted_failures.iter().any(|current| {
                    current.name == failure.name && current.message == failure.message
                }) {
                    self.extracted_failures.push(failure);
                }
            }
        } else {
            self.failures = TestFailureAccumulator::default();
        }
        self.javascript = JavaScriptTestDiagnosticParser::default();
        self.java = JavaTestDiagnosticParser::default();
        self.python = PythonDiagnosticParser::default();
    }

    #[must_use]
    pub fn finish(self) -> TestEvidenceResult {
        if self.extracted_failures.is_empty() {
            return TestEvidenceResult {
                diagnostics: self
                    .fallback
                    .map(|(_, diagnostic)| diagnostic)
                    .into_iter()
                    .collect(),
                ..TestEvidenceResult::default()
            };
        }

        let cases = self
            .extracted_failures
            .iter()
            .take(MAX_FAILURES)
            .map(|failure| TestCase {
                name: failure.name.clone(),
                status: TestStatus::Failed,
                duration_ms: None,
                message: Some(failure.message.clone()),
            })
            .collect();
        let diagnostics = self
            .extracted_failures
            .into_iter()
            .take(MAX_FAILURES)
            .map(|failure| Diagnostic {
                severity: Severity::Error,
                category: DiagnosticCategory::Test,
                message: failure.message,
                location: failure.location,
                target: Some(self.label.clone()),
                action: None,
                repetition_count: 1,
            })
            .collect();
        TestEvidenceResult { cases, diagnostics }
    }
}

pub(crate) fn failure_evidence_priority(line: &str) -> Option<u8> {
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

#[cfg(test)]
mod tests {
    use super::{TestEvidenceInput, TestEvidenceReducer};

    #[test]
    fn reducer_prefers_structured_failures_to_fallback_excerpts() {
        let mut reducer = TestEvidenceReducer::new(TestEvidenceInput {
            label: "//example:test",
        });
        for line in [
            "error: generic failure",
            "test example::fails ... FAILED",
            "---- example::fails stdout ----",
            "thread 'example::fails' panicked at src/lib.rs:7:3:",
            "assertion `left == right` failed",
        ] {
            reducer.observe_line(line);
        }
        reducer.finish_log(true);
        let result = reducer.finish();

        assert_eq!(result.cases.len(), 1);
        assert_eq!(result.cases[0].name, "example::fails");
        assert_eq!(result.diagnostics.len(), 1);
        assert!(result.diagnostics[0].message.contains("assertion"));
    }
}
