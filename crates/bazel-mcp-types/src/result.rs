use serde::{Deserialize, Serialize};

use crate::{CoverageSummary, Diagnostic, QueryRow, TestResult};

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
    pub inspect_hint: Option<String>,
}
