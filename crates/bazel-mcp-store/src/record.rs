//! Explicit compact and hydrated invocation record shapes.

use bazel_mcp_types::{
    CoverageFile, CoverageSummary, Diagnostic, InspectHint, InvocationMetrics, InvocationRecord,
    InvocationRequest, InvocationState, InvocationSummary, QueryRow, RunSummary, TargetCounts,
    TargetResult, Termination, TestCounts, TestResult,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Metadata retained in the manifest and in-memory index.
///
/// Large target, test, and per-file coverage collections are deliberately not
/// represented by this type. Callers that need them must request a
/// [`HydratedInvocation`].
#[derive(Clone, Debug, PartialEq)]
pub struct InvocationHeader {
    pub request: InvocationRequest,
    pub state: InvocationState,
    started_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
    pub termination: Option<Termination>,
    pub summary: Option<InvocationSummaryHeader>,
    run: Option<RunSummary>,
    pub metrics: InvocationMetrics,
    canonical_arguments: Option<Vec<String>>,
    pub(crate) cancellation_reason: Option<String>,
}

impl InvocationHeader {
    #[must_use]
    pub(crate) fn from_record(record: &InvocationRecord) -> Self {
        record.clone().into()
    }
}

impl From<InvocationRecord> for InvocationHeader {
    fn from(record: InvocationRecord) -> Self {
        Self {
            request: record.request,
            state: record.state,
            started_at_ms: record.started_at_ms,
            finished_at_ms: record.finished_at_ms,
            termination: record.termination,
            summary: record.summary.map(InvocationSummaryHeader::from),
            run: record.run,
            metrics: record.metrics,
            canonical_arguments: record.canonical_arguments,
            cancellation_reason: record.cancellation_reason,
        }
    }
}

impl InvocationHeader {
    #[must_use]
    pub fn into_record(self) -> InvocationRecord {
        InvocationRecord {
            request: self.request,
            state: self.state,
            started_at_ms: self.started_at_ms,
            finished_at_ms: self.finished_at_ms,
            termination: self.termination,
            summary: self.summary.map(InvocationSummaryHeader::into_summary),
            run: self.run,
            metrics: self.metrics,
            canonical_arguments: self.canonical_arguments,
            cancellation_reason: self.cancellation_reason,
        }
    }
}

impl From<&InvocationRecord> for InvocationHeader {
    fn from(record: &InvocationRecord) -> Self {
        Self::from_record(record)
    }
}

// The schema-v1 manifest encoded compact records as `InvocationRecord` with
// three empty collections. Serialize through that legacy shape so introducing
// the explicit type does not alter durable JSON.
impl Serialize for InvocationHeader {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.clone().into_record().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for InvocationHeader {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        InvocationRecord::deserialize(deserializer).map(Self::from)
    }
}

/// Summary fields available without loading the detail sidecar.
#[derive(Clone, Debug, PartialEq)]
pub struct InvocationSummaryHeader {
    success: bool,
    pub headline: String,
    pub target_counts: TargetCounts,
    pub(crate) diagnostics: Vec<Diagnostic>,
    pub test_counts: TestCounts,
    coverage: Option<CoverageHeader>,
    query_sample: Vec<QueryRow>,
    pub(crate) query_result_count: Option<u64>,
    elapsed_ms: u64,
    truncated: bool,
    inspect_hint: Option<InspectHint>,
}

impl InvocationSummaryHeader {
    #[must_use]
    fn into_summary(self) -> InvocationSummary {
        InvocationSummary {
            success: self.success,
            headline: self.headline,
            targets: Vec::new(),
            target_counts: self.target_counts,
            diagnostics: self.diagnostics,
            tests: Vec::new(),
            test_counts: self.test_counts,
            coverage: self.coverage.map(CoverageHeader::into_summary),
            query_sample: self.query_sample,
            query_result_count: self.query_result_count,
            elapsed_ms: self.elapsed_ms,
            truncated: self.truncated,
            inspect_hint: self.inspect_hint,
        }
    }
}

impl From<&InvocationSummary> for InvocationSummaryHeader {
    fn from(summary: &InvocationSummary) -> Self {
        summary.clone().into()
    }
}

impl From<InvocationSummary> for InvocationSummaryHeader {
    fn from(summary: InvocationSummary) -> Self {
        Self {
            success: summary.success,
            headline: summary.headline,
            target_counts: summary.target_counts,
            diagnostics: summary.diagnostics,
            test_counts: summary.test_counts,
            coverage: summary.coverage.map(CoverageHeader::from),
            query_sample: summary.query_sample,
            query_result_count: summary.query_result_count,
            elapsed_ms: summary.elapsed_ms,
            truncated: summary.truncated,
            inspect_hint: summary.inspect_hint,
        }
    }
}

/// Aggregate coverage values retained in the compact manifest.
#[derive(Clone, Debug, PartialEq)]
pub struct CoverageHeader {
    lines_found: u64,
    lines_hit: u64,
    coverage_percent: f64,
}

impl CoverageHeader {
    fn into_summary(self) -> CoverageSummary {
        CoverageSummary {
            lines_found: self.lines_found,
            lines_hit: self.lines_hit,
            coverage_percent: self.coverage_percent,
            files: Vec::new(),
        }
    }
}

impl From<&CoverageSummary> for CoverageHeader {
    fn from(summary: &CoverageSummary) -> Self {
        summary.clone().into()
    }
}

impl From<CoverageSummary> for CoverageHeader {
    fn from(summary: CoverageSummary) -> Self {
        Self {
            lines_found: summary.lines_found,
            lines_hit: summary.lines_hit,
            coverage_percent: summary.coverage_percent,
        }
    }
}

/// Large collections stored in `details.json` rather than the manifest.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct InvocationDetails {
    pub(crate) targets: Vec<TargetResult>,
    pub(crate) tests: Vec<TestResult>,
    pub(crate) coverage_files: Vec<CoverageFile>,
}

impl InvocationDetails {
    #[must_use]
    pub(crate) fn from_record(record: &InvocationRecord) -> Self {
        let Some(summary) = &record.summary else {
            return Self::default();
        };
        Self {
            targets: summary.targets.clone(),
            tests: summary.tests.clone(),
            coverage_files: summary
                .coverage
                .as_ref()
                .map_or_else(Vec::new, |coverage| coverage.files.clone()),
        }
    }

    fn hydrate(self, record: &mut InvocationRecord) {
        let Some(summary) = record.summary.as_mut() else {
            return;
        };
        summary.targets = self.targets;
        summary.tests = self.tests;
        if let Some(coverage) = summary.coverage.as_mut() {
            coverage.files = self.coverage_files;
        }
    }

    #[must_use]
    pub(crate) fn is_empty(&self) -> bool {
        self.targets.is_empty() && self.tests.is_empty() && self.coverage_files.is_empty()
    }
}

/// A manifest header combined with its detail sidecar.
#[derive(Clone, Debug, PartialEq)]
pub struct HydratedInvocation {
    pub(crate) header: InvocationHeader,
    pub(crate) details: InvocationDetails,
}

impl HydratedInvocation {
    #[must_use]
    #[cfg(test)]
    fn from_record(record: &InvocationRecord) -> Self {
        Self {
            header: InvocationHeader::from_record(record),
            details: InvocationDetails::from_record(record),
        }
    }

    #[must_use]
    pub(crate) fn into_record(self) -> InvocationRecord {
        let mut record = self.header.into_record();
        self.details.hydrate(&mut record);
        record
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use bazel_mcp_types::{
        BazelCommand, CoverageFile, CoverageSummary, InvocationRequest, InvocationSummary,
        TargetResult, TestResult, TestStatus,
    };

    use super::*;

    fn full_record() -> InvocationRecord {
        let mut record = InvocationRecord::queued(InvocationRequest::new(
            PathBuf::from("/workspace"),
            BazelCommand::Test,
            vec!["//...".into()],
        ));
        record.summary = Some(InvocationSummary {
            targets: vec![TargetResult {
                label: "//pkg:target".into(),
                success: true,
            }],
            tests: vec![TestResult {
                label: "//pkg:test".into(),
                status: TestStatus::Passed,
                duration_ms: Some(1),
                attempts: 1,
                shard: None,
                cases: Vec::new(),
                test_log_available: false,
                test_log_unavailable_reason: None,
            }],
            coverage: Some(CoverageSummary {
                files: vec![CoverageFile {
                    path: "pkg/lib.rs".into(),
                    lines_found: 2,
                    lines_hit: 2,
                    coverage_percent: 100.0,
                }],
                ..CoverageSummary::default()
            }),
            ..InvocationSummary::default()
        });
        record
    }

    #[test]
    fn header_uses_the_schema_v1_compact_json_shape() {
        let full = full_record();
        let header = InvocationHeader::from_record(&full);
        let encoded = serde_json::to_value(&header).unwrap();
        assert_eq!(encoded["summary"]["targets"], serde_json::json!([]));
        assert_eq!(encoded["summary"]["tests"], serde_json::json!([]));
        assert_eq!(
            encoded["summary"]["coverage"]["files"],
            serde_json::json!([])
        );
        let decoded: InvocationHeader = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, header);
    }

    #[test]
    fn hydrated_invocation_round_trips_every_detail_collection() {
        let full = full_record();
        assert_eq!(HydratedInvocation::from_record(&full).into_record(), full);
    }
}
