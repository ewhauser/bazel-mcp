//! Deterministic, bounded reduction of BEP and terminal evidence.

mod budget;
mod build;
mod coverage;
mod diagnostics;
mod extension;
mod query;
mod starlark;
mod test;
mod test_evidence;

pub use budget::{Budget, Budgeted};
pub use build::{
    BepAccumulator, ReductionInput, RunBepOutcome, StreamReductionOutput,
    extract_canonical_arguments, finalize_diagnostics, reduce_artifacts, reduce_invocation,
};
pub use coverage::{CoverageError, parse_lcov_reader};
pub use diagnostics::map_diagnostic as map_text_diagnostic;
pub use extension::{
    CustomReducer, ReducerApplyReport, ReducerContext, ReducerError, ReducerEvent,
    ReducerEventKind, ReducerFailure, ReducerMode, ReducerPatch, ReducerPipeline, ReducerSelector,
};
pub use logcompact_builtins::{deduplicate_lines, normalize_terminal_text};
pub use query::reduce_query;
pub use starlark::{
    REDUCER_API_VERSION, RawStarlarkConfig, StarlarkLimits, StarlarkReducerConfig,
    load_starlark_reducers,
};
pub use test::{TestXmlError, parse_test_xml};
pub use test_evidence::{
    TestEvidenceInput, TestEvidenceReducer, TestEvidenceResult, TestFailureAccumulator,
    TestFailureEvidence,
};
