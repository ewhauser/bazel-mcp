use bazel_mcp_types::{DiagnosticLocation, TestCase, TestStatus};
use quick_xml::de::from_str;
use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TestXmlError {
    #[error(transparent)]
    Xml(#[from] quick_xml::DeError),
}

const MAX_LOG_FAILURES: usize = 20;
const MAX_TEST_NAME_BYTES: usize = 512;
const MAX_FAILURE_MESSAGE_BYTES: usize = 1_000;
const MAX_FAILURE_DETAIL_BYTES: usize = 256;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TestFailureEvidence {
    pub name: String,
    pub message: String,
    pub location: Option<DiagnosticLocation>,
}

/// Streaming, bounded extraction of concrete failed-test evidence from test logs.
///
/// The first supported structured format is Rust's libtest output. Keeping this
/// state machine in the reducer makes selection deterministic without retaining
/// an entire potentially large test log in memory.
#[derive(Default)]
pub struct TestFailureAccumulator {
    failed_names: Vec<String>,
    failures: Vec<TestFailureEvidence>,
    current: Option<RustFailureBlock>,
}

#[derive(Default)]
struct RustFailureBlock {
    name: String,
    location: Option<DiagnosticLocation>,
    assertion: Option<String>,
    panic_message: Option<String>,
    details: Vec<String>,
    saw_panic: bool,
}

impl TestFailureAccumulator {
    pub fn observe_line(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }

        if let Some(name) = rust_failed_status_name(line) {
            push_unique_bounded(
                &mut self.failed_names,
                name,
                MAX_LOG_FAILURES,
                MAX_TEST_NAME_BYTES,
            );
        }

        if let Some(name) = rust_failure_block_name(line) {
            self.finish_current();
            self.current = Some(RustFailureBlock {
                name: bounded_text(name, MAX_TEST_NAME_BYTES),
                ..RustFailureBlock::default()
            });
            return;
        }

        if let Some((name, location)) = rust_panic(line) {
            let replace = self
                .current
                .as_ref()
                .is_some_and(|current| !current.name.is_empty() && current.name != name);
            if replace {
                self.finish_current();
            }
            let current = self.current.get_or_insert_with(RustFailureBlock::default);
            current.name = bounded_text(name, MAX_TEST_NAME_BYTES);
            current.location = location;
            current.saw_panic = true;
            return;
        }

        if is_failure_heading(line) {
            self.finish_current();
            return;
        }

        let Some(current) = self.current.as_mut() else {
            return;
        };
        let lower = line.to_ascii_lowercase();
        if lower.contains("assertion") && lower.contains(" failed") {
            current.assertion = Some(bounded_text(line, MAX_FAILURE_DETAIL_BYTES));
        } else if is_assertion_detail(&lower) {
            if current.details.len() < 4 {
                current
                    .details
                    .push(bounded_text(line, MAX_FAILURE_DETAIL_BYTES));
            }
        } else if current.saw_panic
            && current.panic_message.is_none()
            && !lower.starts_with("note:")
            && !lower.starts_with("stack backtrace:")
            && !lower.starts_with("test result:")
        {
            current.panic_message = Some(bounded_text(line, MAX_FAILURE_DETAIL_BYTES));
        }
    }

    #[must_use]
    pub fn finish(mut self) -> Vec<TestFailureEvidence> {
        self.finish_current();
        for name in self.failed_names {
            if self.failures.len() >= MAX_LOG_FAILURES {
                break;
            }
            if self.failures.iter().any(|failure| failure.name == name) {
                continue;
            }
            self.failures.push(TestFailureEvidence {
                message: format!("Rust test {name} failed"),
                name,
                location: None,
            });
        }
        self.failures
    }

    fn finish_current(&mut self) {
        let Some(current) = self.current.take() else {
            return;
        };
        if current.name.is_empty() || self.failures.len() >= MAX_LOG_FAILURES {
            return;
        }
        let mut message = format!("Rust test {} failed", current.name);
        if let Some(location) = &current.location {
            message.push_str(" at ");
            message.push_str(&location.path);
            if let Some(line) = location.line {
                message.push(':');
                message.push_str(&line.to_string());
            }
            if let Some(column) = location.column {
                message.push(':');
                message.push_str(&column.to_string());
            }
        }
        if let Some(reason) = current.assertion.or(current.panic_message) {
            message.push_str(": ");
            message.push_str(&reason);
        }
        for detail in current.details {
            message.push_str("; ");
            message.push_str(&detail);
        }
        self.failures.push(TestFailureEvidence {
            name: current.name,
            message: bounded_text(&message, MAX_FAILURE_MESSAGE_BYTES),
            location: current.location,
        });
    }
}

#[derive(Debug, Deserialize)]
struct Document {
    #[serde(rename = "testsuite", default)]
    suites: Vec<Suite>,
    #[serde(rename = "testcase", default)]
    cases: Vec<Case>,
}

#[derive(Debug, Deserialize)]
struct Suite {
    #[serde(rename = "testcase", default)]
    cases: Vec<Case>,
}

#[derive(Debug, Deserialize)]
struct Case {
    #[serde(rename = "@name", default)]
    name: String,
    #[serde(rename = "@time")]
    time: Option<f64>,
    failure: Option<Message>,
    error: Option<Message>,
    skipped: Option<Message>,
}

#[derive(Debug, Deserialize)]
struct Message {
    #[serde(rename = "@message")]
    message: Option<String>,
    #[serde(rename = "$text")]
    text: Option<String>,
}

pub fn parse_test_xml(input: &str) -> Result<Vec<TestCase>, TestXmlError> {
    let document: Document = from_str(input)?;
    Ok(document
        .cases
        .into_iter()
        .chain(document.suites.into_iter().flat_map(|suite| suite.cases))
        .map(|case| {
            let (status, detail) = if let Some(message) = case.failure.or(case.error) {
                (TestStatus::Failed, message.message.or(message.text))
            } else if let Some(message) = case.skipped {
                (TestStatus::Skipped, message.message.or(message.text))
            } else {
                (TestStatus::Passed, None)
            };
            TestCase {
                name: case.name,
                status,
                duration_ms: case.time.and_then(duration_ms),
                message: detail,
            }
        })
        .collect())
}

fn duration_ms(seconds: f64) -> Option<u64> {
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    let milliseconds = seconds * 1_000.0;
    if milliseconds >= u64::MAX as f64 {
        Some(u64::MAX)
    } else {
        Some(milliseconds as u64)
    }
}

fn rust_failed_status_name(line: &str) -> Option<&str> {
    line.strip_prefix("test ")?
        .strip_suffix(" ... FAILED")
        .filter(|name| !name.is_empty())
}

fn rust_failure_block_name(line: &str) -> Option<&str> {
    let body = line.strip_prefix("---- ")?.strip_suffix(" ----")?;
    body.strip_suffix(" stdout")
        .or_else(|| body.strip_suffix(" stderr"))
        .filter(|name| !name.is_empty())
}

fn rust_panic(line: &str) -> Option<(&str, Option<DiagnosticLocation>)> {
    let rest = line.strip_prefix("thread '")?;
    let marker = " panicked at ";
    let marker_index = rest.find(marker)?;
    let thread = &rest[..marker_index];
    let quote = thread.rfind('\'')?;
    let name = &thread[..quote];
    if name.is_empty() {
        return None;
    }
    let location = parse_location(&rest[marker_index + marker.len()..]);
    Some((name, location))
}

fn parse_location(value: &str) -> Option<DiagnosticLocation> {
    let value = value.trim().trim_end_matches(':');
    let mut parts = value.rsplitn(3, ':');
    let column = parts.next()?.parse::<u32>().ok()?;
    let line = parts.next()?.parse::<u32>().ok()?;
    let path = parts.next()?.trim();
    if path.is_empty() {
        return None;
    }
    Some(DiagnosticLocation {
        path: bounded_text(path, MAX_FAILURE_DETAIL_BYTES),
        line: Some(line),
        column: Some(column),
    })
}

fn is_assertion_detail(lower: &str) -> bool {
    ["left:", "right:", "expected:", "actual:"]
        .iter()
        .any(|prefix| lower.starts_with(prefix))
}

fn is_failure_heading(line: &str) -> bool {
    matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "failure:" | "failures:"
    )
}

fn push_unique_bounded(
    values: &mut Vec<String>,
    value: &str,
    maximum_items: usize,
    maximum_bytes: usize,
) {
    if values.len() >= maximum_items {
        return;
    }
    let value = bounded_text(value, maximum_bytes);
    if !values.contains(&value) {
        values.push(value);
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
    fn rejects_non_finite_and_negative_test_durations() {
        assert_eq!(duration_ms(-1.0), None);
        assert_eq!(duration_ms(f64::NAN), None);
        assert_eq!(duration_ms(f64::INFINITY), None);
        assert_eq!(duration_ms(1.25), Some(1_250));
        assert_eq!(duration_ms(f64::MAX), Some(u64::MAX));
    }

    #[test]
    fn parses_direct_testsuite_root() {
        let cases = parse_test_xml(
            r#"<testsuite><testcase name="one" time="0.25"><failure message="bad"/></testcase></testsuite>"#,
        )
        .unwrap();
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].name, "one");
        assert_eq!(cases[0].status, TestStatus::Failed);
    }

    #[test]
    fn parses_testsuites_wrapper() {
        let cases = parse_test_xml(
            r#"<testsuites><testsuite><testcase name="one" time="0.25"><failure message="bad"/></testcase></testsuite><testsuite><testcase name="two" time="0.1"/></testsuite></testsuites>"#,
        )
        .unwrap();
        assert_eq!(cases.len(), 2);
        assert_eq!(cases[0].status, TestStatus::Failed);
        assert_eq!(cases[1].status, TestStatus::Passed);
    }

    #[test]
    fn extracts_rust_test_name_assertion_values_and_location() {
        let mut accumulator = TestFailureAccumulator::default();
        for line in [
            "running 3 tests",
            "test build::tests::successful_root_cause_test ... ok",
            "test test::tests::parses_direct_testsuite_root ... FAILED",
            "failures:",
            "---- test::tests::parses_direct_testsuite_root stdout ----",
            "thread 'test::tests::parses_direct_testsuite_root' (3670855) panicked at crates/bazel-mcp-reducer/src/test.rs:101:9:",
            "assertion `left == right` failed",
            "left: \"one\"",
            "right: \"expected\"",
            "failures:",
            "test::tests::parses_direct_testsuite_root",
            "test result: FAILED. 2 passed; 1 failed",
        ] {
            accumulator.observe_line(line);
        }

        let failures = accumulator.finish();
        assert_eq!(failures.len(), 1);
        assert_eq!(
            failures[0].name,
            "test::tests::parses_direct_testsuite_root"
        );
        assert!(
            failures[0]
                .message
                .contains("assertion `left == right` failed")
        );
        assert!(failures[0].message.contains("left: \"one\""));
        assert!(failures[0].message.contains("right: \"expected\""));
        assert_eq!(
            failures[0].location,
            Some(DiagnosticLocation {
                path: "crates/bazel-mcp-reducer/src/test.rs".into(),
                line: Some(101),
                column: Some(9),
            })
        );
        assert!(!failures[0].message.contains("successful_root_cause_test"));
    }

    #[test]
    fn falls_back_to_failed_rust_status_without_a_panic_block() {
        let mut accumulator = TestFailureAccumulator::default();
        accumulator.observe_line("test tests::failed_without_output ... FAILED");
        let failures = accumulator.finish();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].name, "tests::failed_without_output");
        assert_eq!(
            failures[0].message,
            "Rust test tests::failed_without_output failed"
        );
    }
}
