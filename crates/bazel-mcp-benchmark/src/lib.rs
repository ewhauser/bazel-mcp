//! Benchmark corpus, transcript, token accounting, and report support.

mod corpus;
mod harness;
mod report;
mod transcript;

pub use corpus::{ProjectManifest, Scenario};
pub use harness::{HarnessConfig, run_integration};
pub use report::{AdapterMetrics, BenchmarkReport, EnvironmentMetadata, SummaryStatistics};
pub use transcript::{Transcript, TranscriptEvent, TranscriptKind, TranscriptMetrics};
