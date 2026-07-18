mod arbitration;
mod common;
mod cpp;
mod go;
mod java;
mod javascript;
mod protobuf;
mod python;
mod rust;
mod typescript;

use std::collections::BTreeSet;

use crate::{Diagnostic, FallbackPolicy, normalize_terminal_text};

pub use go::parse_diagnostic as parse_go_diagnostic;
pub use java::JavaTestDiagnosticParser;
pub use javascript::JavaScriptTestDiagnosticParser;
pub use python::PythonDiagnosticParser;

/// Static built-in reducer contract. Function pointers keep dispatch and
/// allocation costs identical regardless of how many parser modules exist.
#[derive(Clone, Copy)]
struct TextDiagnosticReducer {
    name: &'static str,
    reduce: for<'a> fn(&str, &mut TextDiagnosticContext<'a>),
}

#[derive(Clone, Copy)]
struct LineDiagnosticReducer {
    name: &'static str,
    enabled: fn(bool) -> bool,
    parse: fn(&str) -> Option<Diagnostic>,
}

struct TextDiagnosticContext<'a> {
    diagnostics: &'a mut Vec<Diagnostic>,
    swc_consumed_lines: BTreeSet<String>,
    has_swc_diagnostics: bool,
    javascript_test_messages: BTreeSet<String>,
    java_test_messages: BTreeSet<String>,
}

impl<'a> TextDiagnosticContext<'a> {
    fn new(diagnostics: &'a mut Vec<Diagnostic>) -> Self {
        Self {
            diagnostics,
            swc_consumed_lines: BTreeSet::new(),
            has_swc_diagnostics: false,
            javascript_test_messages: BTreeSet::new(),
            java_test_messages: BTreeSet::new(),
        }
    }
}

const TEXT_DIAGNOSTIC_REDUCERS: &[TextDiagnosticReducer] = &[
    TextDiagnosticReducer {
        name: "javascript-swc",
        reduce: reduce_swc,
    },
    TextDiagnosticReducer {
        name: "cpp-linker",
        reduce: reduce_cpp_linker,
    },
    TextDiagnosticReducer {
        name: "java-compiler",
        reduce: reduce_java_compiler,
    },
    TextDiagnosticReducer {
        name: "rust-compiler",
        reduce: reduce_rust_compiler,
    },
    TextDiagnosticReducer {
        name: "javascript-test",
        reduce: reduce_javascript_tests,
    },
    TextDiagnosticReducer {
        name: "java-test",
        reduce: reduce_java_tests,
    },
    TextDiagnosticReducer {
        name: "python",
        reduce: arbitration::reduce_python,
    },
];

const LINE_DIAGNOSTIC_REDUCERS: &[LineDiagnosticReducer] = &[
    LineDiagnosticReducer {
        name: "cpp",
        enabled: always,
        parse: cpp::parse_diagnostic,
    },
    LineDiagnosticReducer {
        name: "typescript",
        enabled: always,
        parse: typescript::parse_diagnostic,
    },
    LineDiagnosticReducer {
        name: "protobuf",
        enabled: always,
        parse: protobuf::parse_diagnostic,
    },
    LineDiagnosticReducer {
        name: "go",
        enabled: always,
        parse: go::parse_diagnostic,
    },
];

pub(crate) fn add_text_diagnostics(
    input: &[u8],
    diagnostics: &mut Vec<Diagnostic>,
    fallback: FallbackPolicy,
) {
    let normalized = normalize_terminal_text(input);
    let mut context = TextDiagnosticContext::new(diagnostics);
    for reducer in TEXT_DIAGNOSTIC_REDUCERS {
        debug_assert!(!reducer.name.is_empty());
        (reducer.reduce)(&normalized, &mut context);
    }
    arbitration::reduce_lines(
        &normalized,
        &mut context,
        LINE_DIAGNOSTIC_REDUCERS,
        fallback == FallbackPolicy::Generic,
    );
}

fn reduce_swc(input: &str, context: &mut TextDiagnosticContext<'_>) {
    let mut output = javascript::SwcParseOutput::default();
    javascript::reduce_swc(input, &mut output);
    context.has_swc_diagnostics = !output.diagnostics.is_empty();
    context.diagnostics.append(&mut output.diagnostics);
    context.swc_consumed_lines = output.consumed_lines;
}

fn reduce_cpp_linker(input: &str, context: &mut TextDiagnosticContext<'_>) {
    cpp::reduce_linker(input, context.diagnostics);
}

fn reduce_java_compiler(input: &str, context: &mut TextDiagnosticContext<'_>) {
    java::reduce_compiler(input, context.diagnostics);
}

fn reduce_rust_compiler(input: &str, context: &mut TextDiagnosticContext<'_>) {
    rust::reduce(input, context.diagnostics);
}

fn reduce_javascript_tests(input: &str, context: &mut TextDiagnosticContext<'_>) {
    javascript::reduce_tests(
        input,
        context.diagnostics,
        &mut context.javascript_test_messages,
    );
}

fn reduce_java_tests(input: &str, context: &mut TextDiagnosticContext<'_>) {
    java::reduce_tests(input, context.diagnostics, &mut context.java_test_messages);
}

fn always(_: bool) -> bool {
    true
}

#[cfg(feature = "test-support")]
pub(crate) fn parse_cpp_diagnostic(input: &str) -> Option<Diagnostic> {
    cpp::parse_diagnostic(input)
}

#[cfg(feature = "test-support")]
pub(crate) fn parse_cpp_linker_diagnostic(input: &str) -> Option<Diagnostic> {
    cpp::parse_linker_diagnostic(input)
}

#[cfg(feature = "test-support")]
pub(crate) fn cpp_path_end(input: &str, delimiter: char) -> Option<usize> {
    cpp::path_end(input, delimiter)
}

#[cfg(feature = "test-support")]
pub(crate) fn parse_swc_diagnostics(input: &str) -> Vec<Diagnostic> {
    javascript::parse_swc_diagnostics(input).diagnostics
}

#[cfg(feature = "test-support")]
pub(crate) fn parse_protobuf_diagnostic(input: &str) -> Option<Diagnostic> {
    protobuf::parse_diagnostic(input)
}

#[cfg(feature = "test-support")]
pub(crate) fn parse_typescript_diagnostic(input: &str) -> Option<Diagnostic> {
    typescript::parse_diagnostic(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_contract_keeps_prepass_and_line_precedence_explicit() {
        assert_eq!(
            TEXT_DIAGNOSTIC_REDUCERS
                .iter()
                .map(|reducer| reducer.name)
                .collect::<Vec<_>>(),
            [
                "javascript-swc",
                "cpp-linker",
                "java-compiler",
                "rust-compiler",
                "javascript-test",
                "java-test",
                "python",
            ]
        );
        assert_eq!(
            LINE_DIAGNOSTIC_REDUCERS
                .iter()
                .map(|reducer| reducer.name)
                .collect::<Vec<_>>(),
            ["cpp", "typescript", "protobuf", "go"]
        );
    }

    #[test]
    fn line_registry_preserves_parser_output_contract() {
        let mut diagnostics = Vec::new();
        add_text_diagnostics(
            b"src/looks_like.cc:7:3: error: first parser wins",
            &mut diagnostics,
            FallbackPolicy::Generic,
        );
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(
            diagnostics[0].location.as_ref().unwrap().path,
            "src/looks_like.cc"
        );
        assert_eq!(diagnostics[0].message, "first parser wins");
    }
}
