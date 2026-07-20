use serde::{Deserialize, Serialize, Serializer};

use crate::{
    Artifact, BazelCommand, CoverageFile, Diagnostic, InspectHint, InvocationId, InvocationMetrics,
    InvocationState, QueryRow, RunSummary, TargetCounts, Termination, TestCounts, TestResult,
};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InspectView {
    Summary,
    Metrics,
    Diagnostics,
    Tests,
    TestLog,
    Coverage,
    Artifacts,
    QueryResults,
    Log,
    Invocations,
}

impl InspectView {
    const FOLLOW_UP: [Self; 9] = [
        Self::Summary,
        Self::Metrics,
        Self::Diagnostics,
        Self::Tests,
        Self::TestLog,
        Self::Coverage,
        Self::Artifacts,
        Self::QueryResults,
        Self::Log,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Summary => "summary",
            Self::Metrics => "metrics",
            Self::Diagnostics => "diagnostics",
            Self::Tests => "tests",
            Self::TestLog => "test_log",
            Self::Coverage => "coverage",
            Self::Artifacts => "artifacts",
            Self::QueryResults => "query_results",
            Self::Log => "log",
            Self::Invocations => "invocations",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "summary" => Some(Self::Summary),
            "metrics" => Some(Self::Metrics),
            "diagnostics" => Some(Self::Diagnostics),
            "tests" => Some(Self::Tests),
            "test_log" => Some(Self::TestLog),
            "coverage" => Some(Self::Coverage),
            "artifacts" => Some(Self::Artifacts),
            "query_results" => Some(Self::QueryResults),
            "log" => Some(Self::Log),
            "invocations" => Some(Self::Invocations),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct AvailableViews(Vec<InspectView>);

impl AvailableViews {
    #[must_use]
    pub const fn none() -> Self {
        Self(Vec::new())
    }

    #[must_use]
    pub fn follow_up() -> Self {
        Self(InspectView::FOLLOW_UP.to_vec())
    }
}

impl Default for AvailableViews {
    fn default() -> Self {
        Self::follow_up()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct InspectSummary {
    pub success: bool,
    pub headline: String,
    pub targets: TargetCounts,
    pub tests: TestCounts,
    pub diagnostics: Vec<Diagnostic>,
    pub coverage: Option<InspectCoverageSummary>,
    pub query_result_count: Option<u64>,
    pub query_sample: Vec<QueryRow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<RunSummary>,
    pub elapsed_ms: u64,
    pub truncated: bool,
    pub inspect_hint: Option<InspectHint>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct InspectCoverageSummary {
    pub lines_found: u64,
    pub lines_hit: u64,
    pub coverage_percent: f64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct InspectMetrics {
    pub state: InvocationState,
    pub requested_at_ms: i64,
    pub started_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
    pub termination: Option<Termination>,
    pub metrics: InvocationMetrics,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum InspectCoverageItem {
    File(CoverageFile),
    Unavailable(InspectCoverageUnavailable),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct InspectCoverageUnavailable {
    pub availability_reason: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact: Option<Artifact>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct InvocationLedgerEntry {
    pub invocation_id: InvocationId,
    pub workspace: String,
    pub state: InvocationState,
    pub command: BazelCommand,
    pub arguments: Vec<String>,
    pub arguments_truncated: bool,
    pub requested_at_ms: i64,
    pub finished_at_ms: Option<i64>,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub headline: Option<String>,
    pub targets: Option<TargetCounts>,
    pub tests: Option<TestCounts>,
    pub raw_output_bytes: u64,
    pub model_visible_bytes: u64,
    pub inspect_calls: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum InspectPayload {
    Summary(Vec<InspectSummary>),
    Metrics(Vec<InspectMetrics>),
    Diagnostics(Vec<Diagnostic>),
    Tests(Vec<TestResult>),
    TestLog(Vec<String>),
    Coverage(Vec<InspectCoverageItem>),
    Artifacts(Vec<Artifact>),
    QueryResults(Vec<QueryRow>),
    Log(Vec<String>),
    Invocations(Vec<InvocationLedgerEntry>),
}

impl InspectPayload {
    #[must_use]
    fn view(&self) -> InspectView {
        match self {
            Self::Summary(_) => InspectView::Summary,
            Self::Metrics(_) => InspectView::Metrics,
            Self::Diagnostics(_) => InspectView::Diagnostics,
            Self::Tests(_) => InspectView::Tests,
            Self::TestLog(_) => InspectView::TestLog,
            Self::Coverage(_) => InspectView::Coverage,
            Self::Artifacts(_) => InspectView::Artifacts,
            Self::QueryResults(_) => InspectView::QueryResults,
            Self::Log(_) => InspectView::Log,
            Self::Invocations(_) => InspectView::Invocations,
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Summary(items) => items.len(),
            Self::Metrics(items) => items.len(),
            Self::Diagnostics(items) => items.len(),
            Self::Tests(items) => items.len(),
            Self::TestLog(items) | Self::Log(items) => items.len(),
            Self::Coverage(items) => items.len(),
            Self::Artifacts(items) => items.len(),
            Self::QueryResults(items) => items.len(),
            Self::Invocations(items) => items.len(),
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn truncate(&mut self, len: usize) {
        match self {
            Self::Summary(items) => items.truncate(len),
            Self::Metrics(items) => items.truncate(len),
            Self::Diagnostics(items) => items.truncate(len),
            Self::Tests(items) => items.truncate(len),
            Self::TestLog(items) | Self::Log(items) => items.truncate(len),
            Self::Coverage(items) => items.truncate(len),
            Self::Artifacts(items) => items.truncate(len),
            Self::QueryResults(items) => items.truncate(len),
            Self::Invocations(items) => items.truncate(len),
        }
    }
}

impl Serialize for InspectPayload {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Summary(items) => items.serialize(serializer),
            Self::Metrics(items) => items.serialize(serializer),
            Self::Diagnostics(items) => items.serialize(serializer),
            Self::Tests(items) => items.serialize(serializer),
            Self::TestLog(items) | Self::Log(items) => items.serialize(serializer),
            Self::Coverage(items) => items.serialize(serializer),
            Self::Artifacts(items) => items.serialize(serializer),
            Self::QueryResults(items) => items.serialize(serializer),
            Self::Invocations(items) => items.serialize(serializer),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct InspectResult {
    invocation_id: Option<InvocationId>,
    view: InspectView,
    pub items: InspectPayload,
    total_count: Option<u64>,
    filtered_count: Option<u64>,
    pub next_cursor: Option<String>,
    pub truncated: bool,
    #[serde(skip)]
    start_cursor: Option<String>,
    #[serde(skip)]
    item_cursors: Vec<String>,
}

impl InspectResult {
    #[must_use]
    pub fn new(
        invocation_id: Option<InvocationId>,
        items: InspectPayload,
        total_count: Option<u64>,
        filtered_count: Option<u64>,
        next_cursor: Option<String>,
        truncated: bool,
        item_cursors: Vec<String>,
    ) -> Self {
        let view = items.view();
        Self {
            invocation_id,
            view,
            items,
            total_count,
            filtered_count,
            next_cursor,
            truncated,
            start_cursor: None,
            item_cursors,
        }
    }

    #[must_use]
    pub fn with_start_cursor(mut self, start_cursor: Option<String>) -> Self {
        self.start_cursor = start_cursor;
        self
    }

    pub fn truncate_items(&mut self, len: usize) {
        if len >= self.items.len() {
            return;
        }
        self.items.truncate(len);
        self.truncated = true;
        self.next_cursor = len.checked_sub(1).map_or_else(
            || self.start_cursor.clone(),
            |index| self.item_cursors.get(index).cloned(),
        );
        self.item_cursors.truncate(len);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_views_and_available_views_preserve_the_wire_contract() {
        assert_eq!(
            serde_json::to_value(AvailableViews::follow_up()).unwrap(),
            serde_json::json!([
                "summary",
                "metrics",
                "diagnostics",
                "tests",
                "test_log",
                "coverage",
                "artifacts",
                "query_results",
                "log"
            ])
        );
        for view in InspectView::FOLLOW_UP {
            assert_eq!(InspectView::parse(view.as_str()), Some(view));
        }
        assert_eq!(InspectView::parse("unknown"), None);
    }

    #[test]
    fn packing_cursor_points_after_the_last_emitted_item() {
        let mut result = InspectResult::new(
            None,
            InspectPayload::Log(vec!["one".to_owned(), "two".to_owned()]),
            None,
            None,
            Some("after-two".to_owned()),
            true,
            vec!["after-one".to_owned(), "after-two".to_owned()],
        )
        .with_start_cursor(Some("start".to_owned()));
        result.truncate_items(1);
        assert_eq!(result.next_cursor.as_deref(), Some("after-one"));
        result.truncate_items(0);
        assert_eq!(result.next_cursor.as_deref(), Some("start"));
    }
}
