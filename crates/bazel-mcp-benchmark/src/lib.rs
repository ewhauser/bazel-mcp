//! Benchmark corpus, transcript, token accounting, and report support.

mod agentic;
mod corpus;
mod harness;
mod live_agent;
mod report;
mod transcript;

pub use agentic::{
    AgenticAdapter, AgenticComparison, AgenticConfig, AgenticProjectManifest, AgenticReport,
    AgenticSample, AgenticSummary, AgenticTask, AgenticTaskComparison, AgenticWeightedComparison,
    AgenticWeightedSummary, run_agentic_benchmark,
};
pub use corpus::{ProjectManifest, Scenario};
pub use harness::{HarnessConfig, assert_acceptance_gates, recompute_report, run_integration};
pub use live_agent::{
    CodexLiveConfig, LiveAgentComparison, LiveAgentReport, LiveAgentSample, LiveAgentSummary,
    ProviderUsage, run_codex_live_agent,
};
pub use report::{
    AdapterMetrics, BaselineComparison, BenchmarkReport, EnvironmentMetadata, Estimate,
    SummaryStatistics,
};
pub use transcript::TranscriptMetrics;
