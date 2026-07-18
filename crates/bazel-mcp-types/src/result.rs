use serde::{Deserialize, Serialize};

use crate::{CoverageSummary, Diagnostic, QueryRow, TestResult};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    BuildFailed,
    NotLaunched,
    Succeeded,
    ProgramFailed,
    CancelledDuringBuild,
    CancelledDuringProgram,
    TimedOutDuringBuild,
    TimedOutDuringProgram,
    OutputLimitDuringBuild,
    OutputLimitDuringProgram,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RunSummary {
    pub target: String,
    pub outcome: RunOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program_exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output_excerpt: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InspectHint {
    Diagnostics,
    Tests,
    TestLog,
    Coverage,
    Artifacts,
    QueryResults,
    Log,
}

impl InspectHint {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Diagnostics => "diagnostics",
            Self::Tests => "tests",
            Self::TestLog => "test_log",
            Self::Coverage => "coverage",
            Self::Artifacts => "artifacts",
            Self::QueryResults => "query_results",
            Self::Log => "log",
        }
    }
}

#[cfg(test)]
mod inspect_hint_tests {
    use super::InspectHint;

    #[test]
    fn typed_inspect_hints_preserve_wire_names() {
        assert_eq!(
            serde_json::to_string(&InspectHint::QueryResults).unwrap(),
            "\"query_results\""
        );
        assert_eq!(InspectHint::TestLog.as_str(), "test_log");
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct TargetCounts {
    pub requested: usize,
    pub succeeded: usize,
    pub failed: usize,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct TestCounts {
    pub passed: usize,
    pub failed: usize,
    pub flaky: usize,
    pub skipped: usize,
    pub incomplete: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TargetResult {
    pub label: String,
    pub success: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct InvocationSummary {
    pub success: bool,
    pub headline: String,
    pub targets: Vec<TargetResult>,
    pub target_counts: TargetCounts,
    pub diagnostics: Vec<Diagnostic>,
    pub tests: Vec<TestResult>,
    pub test_counts: TestCounts,
    pub coverage: Option<CoverageSummary>,
    #[serde(default)]
    pub query_sample: Vec<QueryRow>,
    #[serde(default)]
    pub query_result_count: Option<u64>,
    pub elapsed_ms: u64,
    pub truncated: bool,
    pub inspect_hint: Option<InspectHint>,
}
