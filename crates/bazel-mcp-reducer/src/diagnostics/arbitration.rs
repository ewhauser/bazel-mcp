use bazel_mcp_types::{Diagnostic, DiagnosticCategory, Severity};

use crate::deduplicate_lines;

use super::{
    LineDiagnosticReducer, TextDiagnosticContext, cpp, java, javascript, python, starlark,
};

pub(super) fn reduce_python(input: &str, context: &mut TextDiagnosticContext<'_>) {
    let mut parser = python::PythonDiagnosticParser::default();
    for line in input.lines() {
        if claimed_test_exception(line, context) {
            continue;
        }
        if let Some(diagnostic) = parser.observe_line(line) {
            context.diagnostics.push(diagnostic);
        }
    }
}

pub(super) fn reduce_lines(
    input: &str,
    context: &mut TextDiagnosticContext<'_>,
    registry: &[LineDiagnosticReducer],
) {
    let candidates = deduplicate_lines(input);
    let has_strict_dependency_block = candidates.iter().any(|(line, _)| {
        line.to_ascii_lowercase()
            .contains("missing strict dependencies")
    });
    let strict_dependency_count = candidates
        .iter()
        .filter(|(line, _)| super::go::strict_dependency_diagnostic(line).is_some())
        .count();

    for (line, count) in candidates {
        if is_bazel_status_line(&line) {
            continue;
        }
        if context.swc_consumed_lines.contains(line.trim())
            || (context.has_swc_diagnostics && javascript::is_swc_action_wrapper(&line))
        {
            continue;
        }

        let mut parsed = false;
        for reducer in registry {
            debug_assert!(!reducer.name.is_empty());
            if !(reducer.enabled)(has_strict_dependency_block) {
                continue;
            }
            if let Some(mut diagnostic) = (reducer.parse)(&line) {
                diagnostic.repetition_count = count;
                context.diagnostics.push(diagnostic);
                parsed = true;
                break;
            }
        }
        if parsed || claimed_language_line(&line, context) {
            continue;
        }
        if has_strict_dependency_block
            && strict_dependency_count == 0
            && line
                .to_ascii_lowercase()
                .contains("missing strict dependencies")
        {
            context.diagnostics.push(Diagnostic {
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
        context.diagnostics.push(Diagnostic {
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

fn claimed_test_exception(line: &str, context: &TextDiagnosticContext<'_>) -> bool {
    javascript::exception_message(line)
        .is_some_and(|message| context.javascript_test_messages.contains(message))
        || java::exception_message(line)
            .is_some_and(|message| context.java_test_messages.contains(message))
}

fn claimed_language_line(line: &str, context: &TextDiagnosticContext<'_>) -> bool {
    cpp::parse_linker_diagnostic(line).is_some()
        || java::parse_compiler_diagnostic(line).is_some()
        || claimed_test_exception(line, context)
        || starlark::parse_inline_diagnostic(line)
            .is_some_and(|diagnostic| starlark::is_root_cause_message(&diagnostic.message))
        || starlark::error_message(line).is_some()
        || starlark::is_traceback_header(line)
        || python::parse_location(line).is_some()
        || python::exception_message(line).is_some()
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

fn is_bazel_status_line(line: &str) -> bool {
    let lower = line.trim().to_ascii_lowercase();
    lower.starts_with("info:") || lower == "error: build did not complete successfully"
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
