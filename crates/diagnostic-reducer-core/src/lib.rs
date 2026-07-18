//! Provider-neutral, deterministic, bounded log-reduction state machine.
//!
//! This crate owns only the execution-independent mechanics: input framing,
//! scopes, parser lifecycle, provenance, output transformation, arbitration,
//! exact deduplication, ranking, and serialized byte budgets. Parser packs and
//! provider adapters live in downstream crates.

mod finalize;
mod model;
mod policy;
mod session;
mod text;

pub use finalize::finalize_findings;
pub use model::{
    Budget, Diagnostic, DiagnosticClass, EndReason, EvidenceQuality, FallbackPolicy, Limits,
    Location, LogLine, Provenance, Reduction, ReductionOptions, ReductionStats, Scope, ScopeKind,
    SessionOptions, Severity, Stream, TestFailure, TextInput,
};
pub use policy::{
    GenericRanker, NoPathMapping, NoRedaction, OutputPolicy, PathMapper, RankKey, Ranker, Redactor,
};
pub use session::{Emitter, Parser, ParserPlan, ParserPlanError, ReductionSession, ScopeBoundary};
pub use text::{deduplicate_lines, normalize_terminal_text};
