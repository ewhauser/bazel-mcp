//! Deterministic, source-agnostic reduction of compiler, test, and tool text.
//!
//! The crate performs no I/O and has no async, provider protocol, storage, or
//! runner dependency. Callers provide already acquired text and an explicit
//! redaction policy; only normalized and redacted diagnostics are returned.

mod diagnostics;
mod finalize;
mod model;
mod redaction;
mod streaming;
mod test_failures;
mod test_log;
mod text;

pub use diagnostic_reducer_core::{
    Emitter, GenericRanker, OutputPolicy, Parser, ParserPlan, ParserPlanError, RankKey, Ranker,
    ReductionSession, ScopeBoundary,
};
pub use diagnostics::{
    JavaScriptTestDiagnosticParser, JavaTestDiagnosticParser, PythonDiagnosticParser,
    parse_go_diagnostic,
};
pub use model::{
    Budget, Diagnostic, DiagnosticClass, EndReason, EvidenceQuality, FallbackPolicy, Limits,
    Location, LogLine, Provenance, Reduction, ReductionOptions, ReductionStats, Scope, ScopeKind,
    SessionOptions, Severity, Stream, TestFailure, TextInput,
};
pub use redaction::{NoPathMapping, NoRedaction, PathMapper, Redactor};
pub use streaming::{BuiltinDiagnosticParser, BuiltinParserOptions, builtin_parser_plan};
pub use test_failures::{TestFailureAccumulator, TestFailureEvidence};
pub use test_log::{TestLogReducer, TestLogReduction};
pub use text::{deduplicate_lines, normalize_terminal_text};

/// Reduces one or more text inputs with deterministic parser and input order.
#[must_use]
pub fn reduce(
    inputs: &[TextInput<'_>],
    options: &ReductionOptions,
    redactor: &dyn Redactor,
) -> Reduction {
    reduce_with_policy(
        inputs,
        options,
        OutputPolicy::new(redactor, &NoPathMapping, &GenericRanker),
    )
}

/// Reduces batch inputs with caller-owned path mapping, redaction, and ranking.
#[must_use]
pub fn reduce_with_policy(
    inputs: &[TextInput<'_>],
    options: &ReductionOptions,
    policy: OutputPolicy<'_>,
) -> Reduction {
    let mut diagnostics = Vec::new();
    for input in inputs {
        let start = diagnostics.len();
        diagnostics::add_text_diagnostics(input.text, &mut diagnostics, options.fallback);
        for diagnostic in &mut diagnostics[start..] {
            diagnostic.provenance = input.provenance.cloned();
            if diagnostic.location.is_some() && diagnostic.quality == EvidenceQuality::Structured {
                diagnostic.quality = EvidenceQuality::Located;
            }
        }
    }
    finalize::finalize_with_policy(diagnostics, options.budget, policy)
}

#[cfg(feature = "test-support")]
#[doc(hidden)]
pub fn __parse_cpp_diagnostic(input: &str) -> Option<Diagnostic> {
    diagnostics::parse_cpp_diagnostic(input)
}

#[cfg(feature = "test-support")]
#[doc(hidden)]
pub fn __parse_cpp_linker_diagnostic(input: &str) -> Option<Diagnostic> {
    diagnostics::parse_cpp_linker_diagnostic(input)
}

#[cfg(feature = "test-support")]
#[doc(hidden)]
pub fn __cpp_path_end(input: &str, delimiter: char) -> Option<usize> {
    diagnostics::cpp_path_end(input, delimiter)
}

#[cfg(feature = "test-support")]
#[doc(hidden)]
pub fn __parse_swc_diagnostics(input: &str) -> Vec<Diagnostic> {
    diagnostics::parse_swc_diagnostics(input)
}

#[cfg(feature = "test-support")]
#[doc(hidden)]
pub fn __parse_protobuf_diagnostic(input: &str) -> Option<Diagnostic> {
    diagnostics::parse_protobuf_diagnostic(input)
}

#[cfg(feature = "test-support")]
#[doc(hidden)]
pub fn __parse_typescript_diagnostic(input: &str) -> Option<Diagnostic> {
    diagnostics::parse_typescript_diagnostic(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reduces_compiler_and_test_logs_without_provider_objects() {
        let compiler = Provenance::new("compile").with_label("rust");
        let test = Provenance::new("test").with_label("node");
        let inputs = [
            TextInput::new(
                b"error[E0308]: mismatched types\n --> src/lib.rs:7:5\n  |\n7 | value\n  | ^ expected u32, found &str",
            )
            .with_provenance(&compiler),
            TextInput::new(b"TypeError: total is not a function\n    at src/invoice.test.js:8:3")
                .with_provenance(&test),
        ];
        let reduction = reduce(
            &inputs,
            &ReductionOptions {
                budget: Budget::unbounded(),
                ..ReductionOptions::default()
            },
            &NoRedaction,
        );

        assert_eq!(reduction.diagnostics.len(), 2);
        assert_eq!(reduction.diagnostics[0].class, DiagnosticClass::Compiler);
        assert_eq!(
            reduction.diagnostics[0].location.as_ref().unwrap().path,
            "src/lib.rs"
        );
        assert_eq!(reduction.diagnostics[1].class, DiagnosticClass::Test);
        assert_eq!(
            reduction.diagnostics[1].provenance.as_ref().unwrap().source,
            "test"
        );
    }

    #[test]
    fn provenance_keeps_otherwise_identical_inputs_distinct() {
        let first = Provenance::new("stderr");
        let second = Provenance::new("stdout");
        let inputs = [
            TextInput::new(b"src/main.go:4:2: undefined: value").with_provenance(&first),
            TextInput::new(b"src/main.go:4:2: undefined: value").with_provenance(&second),
        ];
        let reduction = reduce(
            &inputs,
            &ReductionOptions {
                budget: Budget::unbounded(),
                ..ReductionOptions::default()
            },
            &NoRedaction,
        );
        assert_eq!(reduction.diagnostics.len(), 2);
    }

    #[test]
    fn redacts_messages_locations_and_provenance_before_return() {
        let provenance = Provenance::new("job-token=source").with_label("token=label");
        let inputs = [
            TextInput::new(b"token=path/main.go:4:2: token=message").with_provenance(&provenance)
        ];
        let redact = |value: &str| value.replace("token=", "[REDACTED]-");
        let reduction = reduce(
            &inputs,
            &ReductionOptions {
                budget: Budget::unbounded(),
                ..ReductionOptions::default()
            },
            &redact,
        );
        let encoded = serde_json::to_string(&reduction).unwrap();
        assert!(!encoded.contains("token="));
        assert!(encoded.contains("[REDACTED]-path/main.go"));
        assert!(encoded.contains("[REDACTED]-message"));
        assert!(encoded.contains("job-[REDACTED]-source"));
    }

    #[test]
    fn fallback_can_be_disabled_without_disabling_structured_parsers() {
        let inputs = [TextInput::new(
            b"generic fatal: wrapper\nsrc/main.go:4:2: undefined: value",
        )];
        let reduction = reduce(
            &inputs,
            &ReductionOptions {
                budget: Budget::unbounded(),
                fallback: FallbackPolicy::Disabled,
            },
            &NoRedaction,
        );
        assert_eq!(reduction.diagnostics.len(), 1);
        assert_eq!(reduction.diagnostics[0].message, "undefined: value");
    }
}
