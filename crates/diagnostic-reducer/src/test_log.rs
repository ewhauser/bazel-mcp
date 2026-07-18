use crate::{
    Diagnostic, DiagnosticClass, EvidenceQuality, JavaScriptTestDiagnosticParser,
    JavaTestDiagnosticParser, Provenance, PythonDiagnosticParser, Severity, TestFailure,
    TestFailureAccumulator, parse_go_diagnostic,
};

const MAX_FAILURES: usize = 20;
const MAX_MESSAGE_BYTES: usize = 1_000;

/// Provider-neutral output for one or more test-log segments.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TestLogReduction {
    pub failures: Vec<TestFailure>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Streaming, bounded orchestration of language and framework test-log parsers.
#[derive(Default)]
pub struct TestLogReducer {
    javascript: JavaScriptTestDiagnosticParser,
    java: JavaTestDiagnosticParser,
    python: PythonDiagnosticParser,
    failures: TestFailureAccumulator,
    extracted_failures: Vec<TestFailure>,
    fallback: Option<(u8, Diagnostic)>,
    segment_provenance: Option<Provenance>,
}

impl TestLogReducer {
    pub fn observe_line(&mut self, line: &str, provenance: &Provenance) {
        self.segment_provenance
            .get_or_insert_with(|| provenance.clone());
        self.failures.observe_line(line);
        let javascript_diagnostic = self.javascript.observe_line(line);
        let java_diagnostic = self.java.observe_line(line);
        let candidate = if let Some(diagnostic) = parse_go_diagnostic(line) {
            Some((0, as_test_diagnostic(diagnostic, provenance)))
        } else if let Some(diagnostic) = javascript_diagnostic {
            Some((0, as_test_diagnostic(diagnostic, provenance)))
        } else if let Some(diagnostic) = java_diagnostic {
            Some((0, as_test_diagnostic(diagnostic, provenance)))
        } else if let Some(diagnostic) = self.python.observe_line(line) {
            Some((0, as_test_diagnostic(diagnostic, provenance)))
        } else {
            failure_evidence_priority(line).map(|priority| {
                (
                    priority,
                    Diagnostic {
                        severity: Severity::Error,
                        class: DiagnosticClass::Test,
                        code: Some("test.fallback".to_owned()),
                        message: bounded_text(line, MAX_MESSAGE_BYTES),
                        location: None,
                        provenance: Some(provenance.clone()),
                        quality: EvidenceQuality::Fallback,
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

    /// Completes a log segment. Incomplete segments do not emit structured
    /// blocks because their terminal confirmation may be missing.
    pub fn finish_log(&mut self, complete: bool) {
        if complete {
            for diagnostic in [self.javascript.finish(), self.java.finish()]
                .into_iter()
                .flatten()
            {
                let provenance = self
                    .segment_provenance
                    .clone()
                    .unwrap_or_else(|| Provenance::new("test-log"));
                let diagnostic = as_test_diagnostic(diagnostic, &provenance);
                if self.fallback.as_ref().is_none_or(|(priority, current)| {
                    *priority > 0
                        || (*priority == 0
                            && diagnostic.location.is_some()
                            && current.location.is_none())
                }) {
                    self.fallback = Some((0, diagnostic));
                }
            }
            let provenance = self
                .segment_provenance
                .clone()
                .unwrap_or_else(|| Provenance::new("test-log"));
            for failure in std::mem::take(&mut self.failures).finish() {
                if self.extracted_failures.len() >= MAX_FAILURES {
                    break;
                }
                let framework = framework_from_message(&failure.message).map(str::to_owned);
                let finding = TestFailure {
                    name: failure.name,
                    message: failure.message,
                    framework,
                    location: failure.location,
                    provenance: Some(provenance.clone()),
                };
                if !self.extracted_failures.iter().any(|current| {
                    current.name == finding.name && current.message == finding.message
                }) {
                    self.extracted_failures.push(finding);
                }
            }
        } else {
            self.failures = TestFailureAccumulator::default();
        }
        self.javascript = JavaScriptTestDiagnosticParser::default();
        self.java = JavaTestDiagnosticParser::default();
        self.python = PythonDiagnosticParser::default();
        self.segment_provenance = None;
    }

    #[must_use]
    pub fn finish(self) -> TestLogReduction {
        if self.extracted_failures.is_empty() {
            return TestLogReduction {
                diagnostics: self
                    .fallback
                    .map(|(_, diagnostic)| diagnostic)
                    .into_iter()
                    .collect(),
                ..TestLogReduction::default()
            };
        }
        let diagnostics = self
            .extracted_failures
            .iter()
            .map(|failure| Diagnostic {
                severity: Severity::Error,
                class: DiagnosticClass::Test,
                code: failure
                    .framework
                    .as_ref()
                    .map(|framework| format!("{framework}.failure")),
                message: failure.message.clone(),
                location: failure.location.clone(),
                provenance: failure.provenance.clone(),
                quality: if failure.location.is_some() {
                    EvidenceQuality::Located
                } else {
                    EvidenceQuality::Structured
                },
                repetition_count: 1,
            })
            .collect();
        TestLogReduction {
            failures: self.extracted_failures,
            diagnostics,
        }
    }
}

fn as_test_diagnostic(mut diagnostic: Diagnostic, provenance: &Provenance) -> Diagnostic {
    diagnostic.class = DiagnosticClass::Test;
    diagnostic.message = bounded_text(&diagnostic.message, MAX_MESSAGE_BYTES);
    diagnostic.provenance = Some(provenance.clone());
    diagnostic.quality = if diagnostic.location.is_some() {
        EvidenceQuality::Located
    } else {
        EvidenceQuality::Structured
    };
    diagnostic
}

fn framework_from_message(message: &str) -> Option<&'static str> {
    if message.starts_with("Rust test ") {
        Some("rust-libtest")
    } else if message.starts_with("C++ test ") {
        Some("gtest")
    } else if message.starts_with("Go test ") {
        Some("go-test")
    } else {
        None
    }
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
        || lower.contains("undefined reference")
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
    use super::*;

    #[test]
    fn extracts_framework_neutral_test_failure() {
        let provenance = Provenance::new("stderr");
        let mut reducer = TestLogReducer::default();
        for line in [
            "test invoice::fails ... FAILED",
            "---- invoice::fails stdout ----",
            "thread 'invoice::fails' panicked at src/lib.rs:7:3:",
            "assertion `left == right` failed",
        ] {
            reducer.observe_line(line, &provenance);
        }
        reducer.finish_log(true);
        let reduction = reducer.finish();
        assert_eq!(reduction.failures.len(), 1);
        assert_eq!(
            reduction.failures[0].framework.as_deref(),
            Some("rust-libtest")
        );
    }

    #[test]
    fn incomplete_log_discards_structured_state() {
        let provenance = Provenance::new("stderr");
        let mut reducer = TestLogReducer::default();
        reducer.observe_line("=== RUN   TestInvoice", &provenance);
        reducer.observe_line("invoice_test.go:7: got 2; want 3", &provenance);
        reducer.finish_log(false);
        assert!(reducer.finish().failures.is_empty());
    }

    #[test]
    fn extracts_node_exception_with_application_frame() {
        let provenance = Provenance::new("stderr");
        let mut reducer = TestLogReducer::default();
        for line in [
            "/tmp/work/cases/runtime_failure.js:2",
            "TypeError: Cannot read properties of undefined",
            "    at invoiceTotal (/tmp/work/cases/runtime_failure.js:2:18)",
            "    at Module._compile (node:internal/modules/cjs/loader:1:1)",
        ] {
            reducer.observe_line(line, &provenance);
        }
        reducer.finish_log(true);
        let reduction = reducer.finish();
        assert!(reduction.failures.is_empty());
        assert_eq!(reduction.diagnostics.len(), 1);
        assert_eq!(
            reduction.diagnostics[0].location.as_ref().unwrap().path,
            "/tmp/work/cases/runtime_failure.js"
        );
    }
}
