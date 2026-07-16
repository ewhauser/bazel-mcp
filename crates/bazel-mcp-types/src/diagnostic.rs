use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
    Note,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticCategory {
    Workspace,
    Loading,
    Analysis,
    Visibility,
    Action,
    Compilation,
    Test,
    Bazel,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DiagnosticLocation {
    pub path: String,
    pub line: Option<u32>,
    pub column: Option<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub severity: Severity,
    pub category: DiagnosticCategory,
    pub message: String,
    pub location: Option<DiagnosticLocation>,
    pub target: Option<String>,
    pub action: Option<String>,
    pub repetition_count: u32,
}
