//! Deterministic, bounded reduction of BEP and terminal evidence.

mod budget;
mod build;
mod coverage;
mod query;
mod test;
mod text;

pub use budget::{Budget, Budgeted};
pub use build::{ReductionInput, extract_canonical_arguments, reduce_artifacts, reduce_invocation};
pub use coverage::{CoverageError, parse_lcov, parse_lcov_reader};
pub use query::reduce_query;
pub use test::{TestXmlError, parse_test_xml};
pub use text::{deduplicate_lines, normalize_terminal_text};
