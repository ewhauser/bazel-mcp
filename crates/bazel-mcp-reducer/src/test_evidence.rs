//! Bazel adapter for provider-neutral streaming test-log reduction.

use bazel_mcp_types::{Diagnostic, DiagnosticCategory, TestCase, TestStatus};
use diagnostic_reducer::{Provenance, TestLogReducer};

use crate::diagnostics::{map_diagnostic, map_path_for_bazel};

const MAX_FAILURES: usize = 20;

pub type TestFailureEvidence = diagnostic_reducer::TestFailureEvidence;

/// Backward-compatible Bazel projection over the provider-neutral accumulator.
#[derive(Default)]
pub struct TestFailureAccumulator(diagnostic_reducer::TestFailureAccumulator);

impl TestFailureAccumulator {
    pub fn observe_line(&mut self, line: &str) {
        self.0.observe_line(line);
    }

    #[must_use]
    pub fn finish(self) -> Vec<TestFailureEvidence> {
        self.0
            .finish()
            .into_iter()
            .map(map_failure_evidence)
            .collect()
    }
}

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

/// Thin Bazel projection over the reusable test-log state machine.
pub struct TestEvidenceReducer {
    label: String,
    reducer: TestLogReducer,
}

impl TestEvidenceReducer {
    #[must_use]
    pub fn new(input: TestEvidenceInput<'_>) -> Self {
        Self {
            label: input.label.to_owned(),
            reducer: TestLogReducer::default(),
        }
    }

    pub fn observe_line(&mut self, line: &str) {
        let provenance = Provenance::new("test-log").with_label(self.label.clone());
        self.reducer.observe_line(line, &provenance);
    }

    pub fn finish_log(&mut self, complete: bool) {
        self.reducer.finish_log(complete);
    }

    #[must_use]
    pub fn finish(self) -> TestEvidenceResult {
        let reduced = self.reducer.finish();
        let failures = reduced
            .failures
            .into_iter()
            .map(map_test_failure)
            .collect::<Vec<_>>();
        if failures.is_empty() {
            return TestEvidenceResult {
                diagnostics: reduced
                    .diagnostics
                    .into_iter()
                    .map(|diagnostic| map_test_diagnostic(diagnostic, &self.label))
                    .collect(),
                ..TestEvidenceResult::default()
            };
        }

        let cases = failures
            .iter()
            .take(MAX_FAILURES)
            .map(|failure| TestCase {
                name: failure.name.clone(),
                status: TestStatus::Failed,
                duration_ms: None,
                message: Some(failure.message.clone()),
            })
            .collect();
        let diagnostics = failures
            .into_iter()
            .take(MAX_FAILURES)
            .map(|failure| Diagnostic {
                severity: bazel_mcp_types::Severity::Error,
                category: DiagnosticCategory::Test,
                message: failure.message,
                location: failure
                    .location
                    .map(|location| bazel_mcp_types::DiagnosticLocation {
                        path: location.path,
                        line: location.line,
                        column: location.column,
                    }),
                target: Some(self.label.clone()),
                action: None,
                repetition_count: 1,
            })
            .collect();
        TestEvidenceResult { cases, diagnostics }
    }
}

fn map_failure_evidence(mut failure: TestFailureEvidence) -> TestFailureEvidence {
    if let Some(location) = &mut failure.location {
        let original = location.path.clone();
        let mapped = map_path_for_bazel(&original);
        if mapped != original {
            failure.message = failure.message.replace(&original, &mapped);
            location.path = mapped;
        }
    }
    failure
}

fn map_test_failure(
    mut failure: diagnostic_reducer::TestFailure,
) -> diagnostic_reducer::TestFailure {
    if let Some(location) = &mut failure.location {
        let original = location.path.clone();
        let mapped = map_path_for_bazel(&original);
        if mapped != original {
            failure.message = failure.message.replace(&original, &mapped);
            location.path = mapped;
        }
    }
    failure
}

fn map_test_diagnostic(diagnostic: diagnostic_reducer::Diagnostic, label: &str) -> Diagnostic {
    let mut diagnostic = map_diagnostic(diagnostic);
    diagnostic.category = DiagnosticCategory::Test;
    diagnostic.target = Some(label.to_owned());
    if let Some(location) = &mut diagnostic.location {
        location.path = map_path_for_bazel(&location.path);
    }
    diagnostic
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

    #[test]
    fn reducer_collapses_duplicate_go_fanout_and_excludes_framework_noise() {
        let mut reducer = TestEvidenceReducer::new(TestEvidenceInput {
            label: "//example:go_test",
        });
        for _ in 0..2 {
            for line in [
                "=== RUN   TestInvoiceTotal",
                "invoice_test.go:18: got 42; want 41",
                "--- FAIL: TestInvoiceTotal (0.00s)",
                "FAIL",
            ] {
                reducer.observe_line(line);
            }
            reducer.finish_log(true);
        }
        let result = reducer.finish();

        assert_eq!(result.cases.len(), 1);
        assert_eq!(result.cases[0].name, "TestInvoiceTotal");
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(
            result.diagnostics[0].category,
            bazel_mcp_types::DiagnosticCategory::Test
        );
        assert_eq!(
            result.diagnostics[0].target.as_deref(),
            Some("//example:go_test")
        );
        assert!(result.diagnostics[0].message.contains("got 42; want 41"));
        assert!(!result.diagnostics[0].message.contains("=== RUN"));
        assert!(!result.diagnostics[0].message.ends_with("FAIL"));
    }
}
