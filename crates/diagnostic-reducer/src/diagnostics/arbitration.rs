use crate::{Diagnostic, DiagnosticClass, Severity};

use crate::deduplicate_lines;

use super::{LineDiagnosticReducer, TextDiagnosticContext, cpp, java, javascript, python, rust};

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
    include_fallbacks: bool,
) {
    let candidates = deduplicate_lines(input);
    for (line, count) in candidates {
        if context.swc_consumed_lines.contains(line.trim())
            || (context.has_swc_diagnostics && javascript::is_swc_action_wrapper(&line))
        {
            continue;
        }

        let mut parsed = false;
        for reducer in registry {
            debug_assert!(!reducer.name.is_empty());
            if !(reducer.enabled)(false) {
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
        if !include_fallbacks {
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
            class: category_from_text(&line),
            code: Some("generic.fallback".to_owned()),
            provenance: None,
            message: line,
            location: None,
            quality: crate::EvidenceQuality::Fallback,
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
        || rust::parse_error_header(line).is_some()
        || claimed_test_exception(line, context)
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
        || lower.contains("error[")
        || lower.starts_with("error ")
        || lower.contains("failed:")
        || lower.contains("undefined reference")
        || lower.contains("fatal:")
        || lower.contains("panicked at")
        || (lower.contains("assertion") && lower.contains(" failed"))
        || lower.starts_with("test result: failed")
        || (line.starts_with("test ") && line.ends_with(" ... FAILED"))
}

fn category_from_text(line: &str) -> DiagnosticClass {
    let lower = line.to_ascii_lowercase();
    if lower.contains("test") || lower.contains("panicked at") || lower.contains("assertion") {
        DiagnosticClass::Test
    } else if lower.contains("error:")
        || lower.contains("error[")
        || lower.contains("undefined reference")
    {
        DiagnosticClass::Compiler
    } else {
        DiagnosticClass::Tool
    }
}
