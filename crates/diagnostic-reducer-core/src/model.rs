use serde::{Deserialize, Serialize};

/// Severity assigned by a parser or structured-input adapter.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
    Note,
}

/// Provider-neutral diagnostic family used by presentation adapters.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticClass {
    Compiler,
    Test,
    Lint,
    Tool,
    Infrastructure,
}

/// How directly a finding is supported by the observed evidence.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceQuality {
    Located,
    #[default]
    Structured,
    Summary,
    Fallback,
}

/// Optional source coordinate extracted from diagnostic text.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct Location {
    pub path: String,
    pub line: Option<u32>,
    pub column: Option<u32>,
}

/// Input channel within a scope.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stream {
    Stdout,
    Stderr,
    #[default]
    Combined,
    Annotation,
    Structured,
}

/// Provider-neutral unit that owns a related log stream.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeKind {
    Invocation,
    Job,
    Step,
    Command,
    Test,
    Attempt,
    #[default]
    Other,
}

/// Caller-owned identity for one independently bounded parser scope.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct Scope {
    pub id: String,
    pub kind: ScopeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl Scope {
    #[must_use]
    pub fn new(id: impl Into<String>, kind: ScopeKind) -> Self {
        Self {
            id: id.into(),
            kind,
            label: None,
        }
    }

    #[must_use]
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    #[must_use]
    pub fn step(id: impl Into<String>) -> Self {
        Self::new(id, ScopeKind::Step)
    }
}

/// Why a parser scope ended.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    Complete,
    Truncated,
    Cancelled,
    Interrupted,
}

/// Caller and state-machine provenance for one returned finding.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct Provenance {
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope_kind: Option<ScopeKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<Stream>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parser: Option<String>,
}

impl Provenance {
    #[must_use]
    pub fn new(source: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            label: None,
            scope_id: None,
            scope_kind: None,
            stream: None,
            start_line: None,
            end_line: None,
            parser: None,
        }
    }

    #[must_use]
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    #[must_use]
    pub fn with_scope(mut self, scope: &Scope) -> Self {
        self.scope_id = Some(scope.id.clone());
        self.scope_kind = Some(scope.kind);
        if self.label.is_none() {
            self.label = scope.label.clone();
        }
        self
    }

    #[must_use]
    pub fn with_span(mut self, stream: Stream, start_line: u64, end_line: u64) -> Self {
        self.stream = Some(stream);
        self.start_line = Some(start_line);
        self.end_line = Some(end_line.max(start_line));
        self
    }

    #[must_use]
    pub fn with_parser(mut self, parser: impl Into<String>) -> Self {
        self.parser = Some(parser.into());
        self
    }
}

/// One normalized and redacted diagnostic.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub severity: Severity,
    pub class: DiagnosticClass,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<Location>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Provenance>,
    #[serde(default, skip_serializing_if = "is_structured_quality")]
    pub quality: EvidenceQuality,
    pub repetition_count: u32,
}

fn is_structured_quality(value: &EvidenceQuality) -> bool {
    *value == EvidenceQuality::Structured
}

/// Structured failed-test evidence independent of a particular test provider.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TestFailure {
    pub name: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub framework: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<Location>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Provenance>,
}

/// Combined item and serialized-finding byte limits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Budget {
    pub max_bytes: usize,
    pub max_items: usize,
}

impl Budget {
    #[must_use]
    pub const fn unbounded() -> Self {
        Self {
            max_bytes: usize::MAX,
            max_items: usize::MAX,
        }
    }
}

/// Controls whether unclaimed actionable lines may become tool diagnostics.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FallbackPolicy {
    Disabled,
    #[default]
    Generic,
}

/// Stable options for one synchronous batch reduction call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReductionOptions {
    pub budget: Budget,
    pub fallback: FallbackPolicy,
}

impl Default for ReductionOptions {
    fn default() -> Self {
        Self {
            budget: Budget {
                max_bytes: 4 * 1024,
                max_items: 20,
            },
            fallback: FallbackPolicy::Generic,
        }
    }
}

/// Hard bounds for incremental input and candidate retention.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    pub max_scopes: usize,
    pub max_scope_bytes: usize,
    pub max_line_bytes: usize,
    pub max_candidates: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_scopes: 128,
            max_scope_bytes: 1024 * 1024,
            max_line_bytes: 64 * 1024,
            max_candidates: 10_000,
        }
    }
}

/// Options for an incremental reduction session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionOptions {
    pub budget: Budget,
    pub limits: Limits,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            budget: ReductionOptions::default().budget,
            limits: Limits::default(),
        }
    }
}

/// Borrowed text plus optional owned output provenance.
#[derive(Clone, Copy, Debug)]
pub struct TextInput<'a> {
    pub text: &'a [u8],
    pub provenance: Option<&'a Provenance>,
}

impl<'a> TextInput<'a> {
    #[must_use]
    pub const fn new(text: &'a [u8]) -> Self {
        Self {
            text,
            provenance: None,
        }
    }

    #[must_use]
    pub const fn with_provenance(mut self, provenance: &'a Provenance) -> Self {
        self.provenance = Some(provenance);
        self
    }
}

/// One normalized input line delivered to an immutable parser plan.
#[derive(Clone, Copy, Debug)]
pub struct LogLine<'a> {
    pub scope: &'a Scope,
    pub stream: Stream,
    pub ordinal: u64,
    pub stream_line: u64,
    pub text: &'a str,
    pub truncated: bool,
}

/// Accounting that never contains raw input text.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReductionStats {
    pub bytes_seen: u64,
    pub bytes_retained: u64,
    pub lines_seen: u64,
    pub truncated_lines: u64,
    pub scopes_started: u64,
    pub scopes_completed: u64,
    pub candidates_emitted: u64,
    pub candidates_dropped: u64,
    pub omitted_test_failures: usize,
}

/// Bounded result and explicit truncation accounting.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Reduction {
    pub diagnostics: Vec<Diagnostic>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub test_failures: Vec<TestFailure>,
    pub truncated: bool,
    pub omitted_diagnostics: usize,
    pub used_bytes: usize,
    #[serde(default, skip_serializing_if = "is_default_stats")]
    pub stats: ReductionStats,
}

fn is_default_stats(value: &ReductionStats) -> bool {
    value == &ReductionStats::default()
}
