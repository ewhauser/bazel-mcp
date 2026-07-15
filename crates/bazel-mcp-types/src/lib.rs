//! Domain types shared by the Bazel MCP subsystems.

mod artifact;
mod command;
mod coverage;
mod diagnostic;
mod invocation;
mod pagination;
mod query;
mod result;
mod test;

pub use artifact::{Artifact, ArtifactKind};
pub use command::{BazelCommand, CommandClass};
pub use coverage::{CoverageFile, CoverageSummary};
pub use diagnostic::{Diagnostic, DiagnosticCategory, DiagnosticLocation, Severity};
pub use invocation::{
    InvocationId, InvocationMetrics, InvocationRecord, InvocationRequest, InvocationState,
    StateTransitionError, Termination, unix_timestamp_ms,
};
pub use pagination::{Page, PageRequest};
pub use query::QueryRow;
pub use result::{InvocationSummary, TargetCounts, TargetResult, TestCounts};
pub use test::{TestCase, TestResult, TestStatus};
