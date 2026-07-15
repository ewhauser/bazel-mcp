use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestStatus {
    Passed,
    Failed,
    Flaky,
    Skipped,
    TimedOut,
    Incomplete,
    Remote,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TestCase {
    pub name: String,
    pub status: TestStatus,
    pub duration_ms: Option<u64>,
    pub message: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TestResult {
    pub label: String,
    pub status: TestStatus,
    pub duration_ms: Option<u64>,
    pub attempts: u32,
    pub shard: Option<u32>,
    pub cases: Vec<TestCase>,
    pub log_uri: Option<String>,
}
