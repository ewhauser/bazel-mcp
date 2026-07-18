use crate::Location;

const MAX_LOG_FAILURES: usize = 20;
const MAX_TEST_NAME_BYTES: usize = 512;
const MAX_FAILURE_MESSAGE_BYTES: usize = 1_000;
const MAX_FAILURE_DETAIL_BYTES: usize = 256;
const MAX_GTEST_DETAILS: usize = 8;
const MAX_GO_DETAILS: usize = 6;
const MAX_GO_BLOCKS: usize = MAX_LOG_FAILURES * 4;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TestFailureEvidence {
    pub name: String,
    pub message: String,
    pub location: Option<Location>,
}

/// Streaming, bounded extraction of concrete failed-test evidence from test logs.
///
/// Supported structured formats include Rust libtest, GoogleTest, and Go's
/// standard test output. Keeping these state machines in the reducer makes
/// selection deterministic without retaining an entire potentially large test
/// log in memory.
#[derive(Default)]
pub struct TestFailureAccumulator {
    failed_names: Vec<String>,
    failures: Vec<TestFailureEvidence>,
    current: Option<RustFailureBlock>,
    gtest_running_name: Option<String>,
    gtest_current: Option<GtestFailureBlock>,
    go_running_name: Option<String>,
    go_panic_name: Option<String>,
    go_blocks: Vec<GoFailureBlock>,
}

#[derive(Default)]
struct RustFailureBlock {
    name: String,
    location: Option<Location>,
    assertion: Option<String>,
    panic_message: Option<String>,
    details: Vec<String>,
    saw_panic: bool,
}

#[derive(Default)]
struct GtestFailureBlock {
    name: String,
    location: Option<Location>,
    details: Vec<String>,
}

#[derive(Default, Eq, PartialEq)]
struct GoFailureBlock {
    name: String,
    location: Option<Location>,
    location_rank: u8,
    assertion: Option<String>,
    panic_message: Option<String>,
    details: Vec<String>,
    failed: bool,
}

impl TestFailureAccumulator {
    pub fn observe_line(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }

        if let Some(name) = go_running_name(line) {
            self.compact_go_blocks();
            self.go_running_name = Some(bounded_text(name, MAX_TEST_NAME_BYTES));
            self.go_panic_name = None;
            return;
        }

        if let Some(name) = go_failed_status_name(line) {
            self.observe_go_failure_status(name);
            return;
        }

        if is_go_nonfailure_status(line) && self.go_running_name.is_some() {
            self.go_panic_name = None;
            self.go_running_name = None;
            return;
        }

        if let Some(message) = go_panic_message(line)
            && self.observe_go_panic(message)
        {
            return;
        }

        if let Some((location, message, stack_frame)) = go_test_location(line)
            && self.observe_go_location(location, message, stack_frame)
        {
            return;
        }

        if self.observe_go_detail(line) {
            return;
        }

        if let Some(name) = gtest_running_name(line) {
            self.finish_gtest_current();
            self.gtest_running_name = Some(bounded_text(name, MAX_TEST_NAME_BYTES));
            return;
        }

        if let Some(location) = gtest_failure_location(line) {
            self.finish_gtest_current();
            self.gtest_current = Some(GtestFailureBlock {
                name: self
                    .gtest_running_name
                    .clone()
                    .unwrap_or_else(|| "<unknown gtest>".to_owned()),
                location,
                ..GtestFailureBlock::default()
            });
            return;
        }

        if let Some(name) = gtest_failed_name(line) {
            if self
                .gtest_running_name
                .as_deref()
                .is_some_and(|running| running == name)
            {
                self.finish_gtest_current();
                self.gtest_running_name = None;
            }
            return;
        }

        if let Some(current) = self.gtest_current.as_mut() {
            if current.details.len() < MAX_GTEST_DETAILS && !is_gtest_framework_status(line) {
                current
                    .details
                    .push(bounded_text(line, MAX_FAILURE_DETAIL_BYTES));
            }
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
        self.finish_gtest_current();
        self.finish_go_failures();
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

    fn finish_gtest_current(&mut self) {
        let Some(current) = self.gtest_current.take() else {
            return;
        };
        if current.name.is_empty() || self.failures.len() >= MAX_LOG_FAILURES {
            return;
        }
        let mut message = format!("C++ test {} failed", current.name);
        if !current.details.is_empty() {
            message.push_str(": ");
            for (index, detail) in current.details.iter().enumerate() {
                if index > 0 {
                    if message.ends_with(':') {
                        message.push(' ');
                    } else {
                        message.push_str("; ");
                    }
                }
                message.push_str(detail);
            }
        }
        self.failures.push(TestFailureEvidence {
            name: current.name,
            message: bounded_text(&message, MAX_FAILURE_MESSAGE_BYTES),
            location: current.location,
        });
    }

    fn observe_go_failure_status(&mut self, name: &str) {
        let name = bounded_text(name, MAX_TEST_NAME_BYTES);
        let index = self
            .go_blocks
            .iter()
            .rposition(|block| block.name == name && !block.failed)
            .or_else(|| self.push_go_block(&name));
        if let Some(index) = index {
            self.go_blocks[index].failed = true;
        }
        self.go_running_name = Some(name.clone());
        self.go_panic_name = Some(name);
    }

    fn observe_go_panic(&mut self, message: &str) -> bool {
        let Some(name) = self
            .go_panic_name
            .clone()
            .or_else(|| self.go_running_name.clone())
        else {
            return false;
        };
        let Some(index) = self.go_block_index(&name, true) else {
            return true;
        };
        let block = &mut self.go_blocks[index];
        if block.panic_message.is_none() {
            block.panic_message = Some(bounded_text(message, MAX_FAILURE_DETAIL_BYTES));
        }
        self.go_panic_name = Some(name);
        true
    }

    fn observe_go_location(
        &mut self,
        location: Location,
        message: Option<&str>,
        stack_frame: bool,
    ) -> bool {
        let panic_context = self.go_panic_name.is_some();
        let Some(name) = self
            .go_panic_name
            .clone()
            .or_else(|| self.go_running_name.clone())
        else {
            return false;
        };
        if stack_frame && is_go_runtime_path(&location.path) {
            return true;
        }
        let Some(index) = self.go_block_index(&name, panic_context) else {
            return true;
        };
        let block = &mut self.go_blocks[index];
        let rank = if stack_frame && !location.path.ends_with("_test.go") {
            1
        } else {
            0
        };
        if block.location.is_none() || rank < block.location_rank {
            block.location = Some(location);
            block.location_rank = rank;
        }
        if let Some(message) = message.filter(|message| !message.is_empty()) {
            if block.assertion.is_none() {
                block.assertion = Some(bounded_text(message, MAX_FAILURE_DETAIL_BYTES));
            } else if is_go_assertion_detail(message) {
                push_unique_bounded(
                    &mut block.details,
                    message,
                    MAX_GO_DETAILS,
                    MAX_FAILURE_DETAIL_BYTES,
                );
            }
        }
        true
    }

    fn observe_go_detail(&mut self, line: &str) -> bool {
        let panic_context = self.go_panic_name.is_some();
        let Some(name) = self
            .go_panic_name
            .clone()
            .or_else(|| self.go_running_name.clone())
        else {
            return false;
        };
        if is_go_framework_noise(line) || looks_like_go_stack_frame(line) {
            return false;
        }
        let existing = self
            .go_blocks
            .iter()
            .rposition(|block| block.name == name && (panic_context || !block.failed));
        if !is_go_assertion_detail(line)
            && !existing.is_some_and(|index| {
                let block = &self.go_blocks[index];
                block.location.is_some() && block.assertion.is_none()
            })
        {
            return false;
        }
        let Some(index) = existing.or_else(|| self.push_go_block(&name)) else {
            return false;
        };
        let block = &mut self.go_blocks[index];
        if is_go_assertion_detail(line) {
            if block.assertion.is_none() {
                block.assertion = Some(bounded_text(line, MAX_FAILURE_DETAIL_BYTES));
            } else {
                push_unique_bounded(
                    &mut block.details,
                    line,
                    MAX_GO_DETAILS,
                    MAX_FAILURE_DETAIL_BYTES,
                );
            }
            return true;
        }
        if block.location.is_some() && block.assertion.is_none() {
            block.assertion = Some(bounded_text(line, MAX_FAILURE_DETAIL_BYTES));
            return true;
        }
        false
    }

    fn go_block_index(&mut self, name: &str, include_failed: bool) -> Option<usize> {
        self.go_blocks
            .iter()
            .rposition(|block| block.name == name && (include_failed || !block.failed))
            .or_else(|| self.push_go_block(name))
    }

    fn push_go_block(&mut self, name: &str) -> Option<usize> {
        if self.go_blocks.len() >= MAX_GO_BLOCKS {
            self.compact_go_blocks();
            if self.go_blocks.len() >= MAX_GO_BLOCKS {
                return None;
            }
        }
        self.go_blocks.push(GoFailureBlock {
            name: bounded_text(name, MAX_TEST_NAME_BYTES),
            location_rank: u8::MAX,
            ..GoFailureBlock::default()
        });
        Some(self.go_blocks.len() - 1)
    }

    fn compact_go_blocks(&mut self) {
        let mut unique = Vec::with_capacity(self.go_blocks.len());
        for block in std::mem::take(&mut self.go_blocks) {
            if block.failed
                && unique
                    .iter()
                    .any(|current: &GoFailureBlock| current.failed && current == &block)
            {
                continue;
            }
            unique.push(block);
        }
        self.go_blocks = unique;
    }

    fn finish_go_failures(&mut self) {
        for block in std::mem::take(&mut self.go_blocks)
            .into_iter()
            .filter(|block| block.failed)
        {
            if self.failures.len() >= MAX_LOG_FAILURES {
                break;
            }
            let mut message = if block.panic_message.is_some() {
                format!("Go test {} panicked", block.name)
            } else {
                format!("Go test {} failed", block.name)
            };
            if let Some(location) = &block.location {
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
            if let Some(reason) = block.panic_message.or(block.assertion) {
                message.push_str(": ");
                message.push_str(&reason);
            }
            for detail in block.details {
                message.push_str("; ");
                message.push_str(&detail);
            }
            let evidence = TestFailureEvidence {
                name: block.name,
                message: bounded_text(&message, MAX_FAILURE_MESSAGE_BYTES),
                location: block.location,
            };
            if !self.failures.iter().any(|current| current == &evidence) {
                self.failures.push(evidence);
            }
        }
    }
}

fn rust_failed_status_name(line: &str) -> Option<&str> {
    line.strip_prefix("test ")?
        .strip_suffix(" ... FAILED")
        .filter(|name| !name.is_empty())
}

fn gtest_running_name(line: &str) -> Option<&str> {
    line.strip_prefix("[ RUN      ] ")
        .filter(|name| !name.is_empty())
}

fn gtest_failed_name(line: &str) -> Option<&str> {
    let name = line.strip_prefix("[  FAILED  ] ")?;
    let name = name.rsplit_once(" (").map_or(name, |(name, duration)| {
        duration
            .strip_suffix(')')
            .filter(|duration| duration.ends_with(" ms"))
            .map_or(name, |_| name)
    });
    (!name.is_empty()).then_some(name)
}

fn go_running_name(line: &str) -> Option<&str> {
    line.strip_prefix("=== RUN")
        .map(str::trim)
        .filter(|name| !name.is_empty())
}

fn go_failed_status_name(line: &str) -> Option<&str> {
    let name = line.strip_prefix("--- FAIL:")?.trim();
    let name = name.rsplit_once(" (").map_or(name, |(name, duration)| {
        duration.strip_suffix(')').map_or(name, |_| name)
    });
    (!name.is_empty()).then_some(name)
}

fn is_go_nonfailure_status(line: &str) -> bool {
    line.starts_with("--- PASS:") || line.starts_with("--- SKIP:")
}

fn go_panic_message(line: &str) -> Option<&str> {
    let message = line.strip_prefix("panic:")?.trim();
    let message = [" [recovered, repanicked]", " [recovered]"]
        .into_iter()
        .find_map(|suffix| message.strip_suffix(suffix))
        .unwrap_or(message)
        .trim();
    (!message.is_empty()).then_some(message)
}

fn go_test_location(line: &str) -> Option<(Location, Option<&str>, bool)> {
    let marker = line.rfind(".go:")?;
    let path_end = marker + ".go".len();
    let raw_path = line[..path_end].trim();
    let error_trace = raw_path.strip_prefix("Error Trace:").map(str::trim);
    let path = error_trace.unwrap_or(raw_path);
    if path.is_empty() {
        return None;
    }

    let remainder = &line[path_end + 1..];
    let digit_count = remainder.bytes().take_while(u8::is_ascii_digit).count();
    if digit_count == 0 {
        return None;
    }
    let line_number = remainder[..digit_count].parse::<u32>().ok()?;
    let mut remainder = &remainder[digit_count..];
    let mut column = None;
    let mut stack_frame = error_trace.is_none() && !remainder.starts_with(':');
    let mut message = None;
    if let Some(after_colon) = remainder.strip_prefix(':') {
        remainder = after_colon;
        let column_digits = remainder.bytes().take_while(u8::is_ascii_digit).count();
        if column_digits > 0 && remainder[column_digits..].starts_with(':') {
            column = remainder[..column_digits].parse::<u32>().ok();
            remainder = &remainder[column_digits + 1..];
        }
        let text = remainder.trim();
        if !text.is_empty() {
            stack_frame = text.starts_with("+0x");
            if !stack_frame {
                message = Some(text);
            }
        }
    }

    Some((
        Location {
            path: bounded_text(&compact_test_path(path), MAX_FAILURE_DETAIL_BYTES),
            line: Some(line_number),
            column,
        },
        message,
        stack_frame,
    ))
}

fn is_go_runtime_path(path: &str) -> bool {
    let absolute = path.starts_with('/') || path.as_bytes().get(1) == Some(&b':');
    absolute
        && ["/src/runtime/", "/src/testing/"]
            .iter()
            .any(|component| path.contains(component))
}

fn is_go_assertion_detail(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    [
        "got:",
        "want:",
        "expected:",
        "actual:",
        "error:",
        "fatal:",
        "message:",
        "messages:",
        "diff:",
    ]
    .iter()
    .any(|prefix| lower.starts_with(prefix))
}

fn is_go_framework_noise(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    line.starts_with("=== ")
        || line.starts_with("--- ")
        || line.starts_with("goroutine ")
        || line.starts_with("created by ")
        || lower == "pass"
        || lower == "fail"
        || lower.starts_with("ok\t")
        || lower.starts_with("?\t")
        || lower.starts_with("coverage:")
        || lower.starts_with("exit status ")
}

fn looks_like_go_stack_frame(line: &str) -> bool {
    (line.contains('(') && line.split_whitespace().count() == 1)
        || line.starts_with("testing.")
        || line.starts_with("runtime.")
}

fn gtest_failure_location(line: &str) -> Option<Option<Location>> {
    let location = line.strip_suffix(": Failure")?;
    if location == "unknown file" {
        return Some(None);
    }
    let (path, line_number) = location.rsplit_once(':')?;
    let line_number = line_number.parse::<u32>().ok()?;
    (!path.is_empty()).then(|| {
        Some(Location {
            path: compact_test_path(path),
            line: Some(line_number),
            column: None,
        })
    })
}

fn compact_test_path(path: &str) -> String {
    let path = path.trim_matches('"').replace('\\', "/");
    path.strip_prefix("./").unwrap_or(&path).to_owned()
}

fn is_gtest_framework_status(line: &str) -> bool {
    [
        "[==========]",
        "[----------]",
        "[  PASSED  ]",
        "[  SKIPPED ]",
    ]
    .iter()
    .any(|prefix| line.starts_with(prefix))
}

fn rust_failure_block_name(line: &str) -> Option<&str> {
    let body = line.strip_prefix("---- ")?.strip_suffix(" ----")?;
    body.strip_suffix(" stdout")
        .or_else(|| body.strip_suffix(" stderr"))
        .filter(|name| !name.is_empty())
}

fn rust_panic(line: &str) -> Option<(&str, Option<Location>)> {
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

fn parse_location(value: &str) -> Option<Location> {
    let value = value.trim().trim_end_matches(':');
    let mut parts = value.rsplitn(3, ':');
    let column = parts.next()?.parse::<u32>().ok()?;
    let line = parts.next()?.parse::<u32>().ok()?;
    let path = parts.next()?.trim();
    if path.is_empty() {
        return None;
    }
    Some(Location {
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
    let ellipsis_bytes = '…'.len_utf8();
    if maximum_bytes < ellipsis_bytes {
        let mut boundary = maximum_bytes;
        while !value.is_char_boundary(boundary) {
            boundary -= 1;
        }
        return value[..boundary].to_owned();
    }
    let mut boundary = maximum_bytes - ellipsis_bytes;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    format!("{}…", &value[..boundary])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_rust_test_name_assertion_values_and_location() {
        let mut accumulator = TestFailureAccumulator::default();
        for line in [
            "running 3 tests",
            "test build::tests::successful_root_cause_test ... ok",
            "test test::tests::parses_direct_testsuite_root ... FAILED",
            "failures:",
            "---- test::tests::parses_direct_testsuite_root stdout ----",
            "thread 'test::tests::parses_direct_testsuite_root' panicked at src/test.rs:101:9:",
            "assertion `left == right` failed",
            "left: \"one\"",
            "right: \"expected\"",
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
        assert_eq!(
            failures[0].location,
            Some(Location {
                path: "src/test.rs".into(),
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

    #[test]
    fn extracts_gtest_assertion_details_and_locations() {
        let mut accumulator = TestFailureAccumulator::default();
        for line in [
            "[ RUN      ] InvoiceTest.IncludesServiceFee",
            "mcp/cpp_fixture/assertion_failure_test.cc:8: Failure",
            "Expected equality of these values:",
            "CalculateInvoiceTotal(40, 2)",
            "Which is: 42",
            "41",
            "invoice total should include the service fee",
            "[  FAILED  ] InvoiceTest.IncludesServiceFee (0 ms)",
        ] {
            accumulator.observe_line(line);
        }
        let failures = accumulator.finish();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].name, "InvoiceTest.IncludesServiceFee");
        assert!(failures[0].message.contains("Which is: 42"));
        assert_eq!(
            failures[0].location,
            Some(Location {
                path: "mcp/cpp_fixture/assertion_failure_test.cc".into(),
                line: Some(8),
                column: None,
            })
        );
    }

    #[test]
    fn extracts_gtest_cpp_exceptions_without_unknown_file_noise() {
        let mut accumulator = TestFailureAccumulator::default();
        for line in [
            "[ RUN      ] InvoiceTest.RejectsMissingCurrency",
            "unknown file: Failure",
            "C++ exception with description \"invoice currency was not configured\" thrown in the test body.",
            "[  FAILED  ] InvoiceTest.RejectsMissingCurrency (0 ms)",
        ] {
            accumulator.observe_line(line);
        }
        let failures = accumulator.finish();
        assert_eq!(failures.len(), 1);
        assert!(
            failures[0]
                .message
                .contains("invoice currency was not configured")
        );
        assert_eq!(failures[0].location, None);
        assert!(!failures[0].message.contains("unknown file"));
    }

    #[test]
    fn bounds_go_failure_items_names_locations_and_messages() {
        let mut accumulator = TestFailureAccumulator::default();
        let path = format!("{}failure_test.go", "very-long-segment/".repeat(24));
        for index in 0..25 {
            let name = format!("Test{index:02}_{}", "name".repeat(160));
            accumulator.observe_line(&format!("=== RUN   {name}"));
            accumulator.observe_line(&format!("{path}:7: assertion {}", "message".repeat(80)));
            accumulator.observe_line(&format!("--- FAIL: {name} (0.00s)"));
        }
        let failures = accumulator.finish();
        assert_eq!(failures.len(), MAX_LOG_FAILURES);
        for failure in failures {
            assert!(failure.name.len() <= MAX_TEST_NAME_BYTES);
            assert!(failure.message.len() <= MAX_FAILURE_MESSAGE_BYTES);
            assert!(failure.location.unwrap().path.len() <= MAX_FAILURE_DETAIL_BYTES);
        }
    }

    #[test]
    fn repeated_go_fanout_does_not_consume_the_unique_failure_budget() {
        let mut accumulator = TestFailureAccumulator::default();
        for _ in 0..(MAX_GO_BLOCKS + 5) {
            for line in [
                "=== RUN   TestRepeated",
                "repeat_test.go:4: got false; want true",
                "--- FAIL: TestRepeated (0.00s)",
            ] {
                accumulator.observe_line(line);
            }
        }
        for line in [
            "=== RUN   TestUnique",
            "unique_test.go:8: fatal: unique failure",
            "--- FAIL: TestUnique (0.00s)",
        ] {
            accumulator.observe_line(line);
        }
        let failures = accumulator.finish();
        assert_eq!(failures.len(), 2);
        assert_eq!(failures[0].name, "TestRepeated");
        assert_eq!(failures[1].name, "TestUnique");
    }
}
