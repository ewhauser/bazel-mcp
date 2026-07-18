mod bazel;

use bazel::add_bazel_diagnostics;

use bazel_mcp_types::{Diagnostic, DiagnosticCategory, DiagnosticLocation, Severity};
use diagnostic_reducer::{
    Budget as CoreBudget, Diagnostic as CoreDiagnostic, DiagnosticClass, GenericRanker,
    NoRedaction, OutputPolicy, ReductionOptions, TextInput, normalize_terminal_text,
    reduce_with_policy,
};

pub(crate) fn add_text_diagnostics(input: &[u8], diagnostics: &mut Vec<Diagnostic>) {
    let normalized = normalize_terminal_text(input);
    let mut bazel_diagnostics = Vec::new();
    add_bazel_diagnostics(&normalized, &mut bazel_diagnostics);
    let generic_input = normalized
        .lines()
        .filter(|line| !bazel::is_bazel_owned_line(line))
        .collect::<Vec<_>>()
        .join("\n");
    let reduction = reduce_with_policy(
        &[TextInput::new(generic_input.as_bytes())],
        &ReductionOptions {
            budget: CoreBudget::unbounded(),
            ..ReductionOptions::default()
        },
        OutputPolicy::new(&NoRedaction, &bazel::BazelPathMapper, &GenericRanker),
    );
    if reduction
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code.as_deref() == Some("swc.parser"))
    {
        bazel_diagnostics.retain(|diagnostic| {
            let lower = diagnostic.message.to_ascii_lowercase();
            !(lower.contains("error executing ") && lower.contains(" command"))
        });
    }
    diagnostics.extend(bazel_diagnostics);
    diagnostics.extend(reduction.diagnostics.into_iter().map(map_diagnostic));
}

pub fn map_diagnostic(diagnostic: CoreDiagnostic) -> Diagnostic {
    let category = match diagnostic.code.as_deref() {
        Some("starlark.loading" | "tool.loading") => DiagnosticCategory::Loading,
        Some("starlark.analysis" | "tool.analysis") => DiagnosticCategory::Analysis,
        Some("tool.visibility") => DiagnosticCategory::Visibility,
        _ => match diagnostic.class {
            DiagnosticClass::Compiler => DiagnosticCategory::Compilation,
            DiagnosticClass::Test => DiagnosticCategory::Test,
            DiagnosticClass::Lint => DiagnosticCategory::Compilation,
            DiagnosticClass::Infrastructure => DiagnosticCategory::Unknown,
            DiagnosticClass::Tool => DiagnosticCategory::Unknown,
            _ => DiagnosticCategory::Unknown,
        },
    };
    Diagnostic {
        severity: match diagnostic.severity {
            diagnostic_reducer::Severity::Error => Severity::Error,
            diagnostic_reducer::Severity::Warning => Severity::Warning,
            diagnostic_reducer::Severity::Note => Severity::Note,
        },
        category,
        message: diagnostic.message,
        location: diagnostic.location.map(|location| DiagnosticLocation {
            path: bazel::compact_bazel_path(&location.path),
            line: location.line,
            column: location.column,
        }),
        target: None,
        action: None,
        repetition_count: diagnostic.repetition_count,
    }
}

pub(crate) fn bounded_text(value: &str, maximum_bytes: usize) -> String {
    if value.len() <= maximum_bytes {
        return value.to_owned();
    }
    let mut boundary = maximum_bytes;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    format!("{}…", &value[..boundary])
}

pub(crate) fn map_path_for_bazel(path: &str) -> String {
    bazel::compact_bazel_path(path)
}
#[cfg(test)]
pub(crate) fn parse_cpp_diagnostic(input: &str) -> Option<Diagnostic> {
    diagnostic_reducer::__parse_cpp_diagnostic(input).map(map_diagnostic)
}

#[cfg(test)]
pub(crate) fn parse_cpp_linker_diagnostic(input: &str) -> Option<Diagnostic> {
    diagnostic_reducer::__parse_cpp_linker_diagnostic(input).map(map_diagnostic)
}

#[cfg(test)]
pub(crate) fn cpp_path_end(input: &str, delimiter: char) -> Option<usize> {
    diagnostic_reducer::__cpp_path_end(input, delimiter)
}

#[cfg(test)]
pub(crate) struct SwcParseOutput {
    pub(crate) diagnostics: Vec<Diagnostic>,
}

#[cfg(test)]
pub(crate) fn parse_swc_diagnostics(input: &str) -> SwcParseOutput {
    SwcParseOutput {
        diagnostics: diagnostic_reducer::__parse_swc_diagnostics(input)
            .into_iter()
            .map(map_diagnostic)
            .collect(),
    }
}

#[cfg(test)]
pub(crate) fn parse_protobuf_diagnostic(input: &str) -> Option<Diagnostic> {
    diagnostic_reducer::__parse_protobuf_diagnostic(input).map(map_diagnostic)
}

#[cfg(test)]
pub(crate) fn parse_starlark_inline_diagnostic(input: &str) -> Option<Diagnostic> {
    bazel::parse_starlark_inline(input)
}

#[cfg(test)]
pub(crate) fn parse_typescript_diagnostic(input: &str) -> Option<Diagnostic> {
    diagnostic_reducer::__parse_typescript_diagnostic(input).map(map_diagnostic)
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct PythonDiagnosticParser(diagnostic_reducer::PythonDiagnosticParser);

#[cfg(test)]
impl PythonDiagnosticParser {
    pub(crate) fn observe_line(&mut self, line: &str) -> Option<Diagnostic> {
        self.0.observe_line(line).map(map_diagnostic)
    }
}

#[cfg(test)]
mod tests {
    use bazel_mcp_types::DiagnosticCategory;

    use super::add_text_diagnostics;

    #[test]
    fn adapter_owns_bazel_status_suppression_and_fallback_mapping() {
        let mut diagnostics = Vec::new();
        add_text_diagnostics(
            b"INFO: 1 process\nERROR: Build did not complete successfully\nERROR: no such target '//pkg:missing'",
            &mut diagnostics,
        );

        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].category, DiagnosticCategory::Loading);
        assert!(diagnostics[0].message.contains("no such target"));
    }
}
