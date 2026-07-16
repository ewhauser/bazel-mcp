//! Deterministic, bounded reduction of BEP and terminal evidence.

mod budget;
mod build;
mod coverage;
mod extension;
mod query;
mod starlark;
mod test;
mod text;

pub use budget::{Budget, Budgeted};
pub use build::{
    BepAccumulator, JavaScriptTestDiagnosticParser, JavaTestDiagnosticParser,
    PythonDiagnosticParser, ReductionInput, StreamReductionOutput, extract_canonical_arguments,
    finalize_diagnostics, parse_go_diagnostic, reduce_artifacts, reduce_invocation,
};
pub use coverage::{CoverageError, parse_lcov, parse_lcov_reader};
pub use extension::{
    CustomReducer, ReducerApplyReport, ReducerContext, ReducerError, ReducerEvent,
    ReducerEventKind, ReducerFailure, ReducerMode, ReducerPatch, ReducerPipeline, ReducerSelector,
};
pub use query::reduce_query;
pub use starlark::{
    REDUCER_API_VERSION, StarlarkLimits, StarlarkReducerConfig, load_starlark_reducers,
};
pub use test::{TestFailureAccumulator, TestFailureEvidence, TestXmlError, parse_test_xml};
pub use text::{deduplicate_lines, normalize_terminal_text};
