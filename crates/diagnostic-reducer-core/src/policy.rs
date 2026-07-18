use crate::{Diagnostic, DiagnosticClass, EvidenceQuality, Severity};

/// Infallible transformation applied to every untrusted returned string.
pub trait Redactor {
    fn redact(&self, value: &str) -> String;
}

impl<F> Redactor for F
where
    F: Fn(&str) -> String,
{
    fn redact(&self, value: &str) -> String {
        self(value)
    }
}

/// Explicit identity redactor for already-sanitized or non-sensitive inputs.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoRedaction;

impl Redactor for NoRedaction {
    fn redact(&self, value: &str) -> String {
        value.to_owned()
    }
}

/// Pure provider-supplied path normalization performed before redaction.
pub trait PathMapper {
    fn map_path(&self, value: &str) -> String;
}

impl<F> PathMapper for F
where
    F: Fn(&str) -> String,
{
    fn map_path(&self, value: &str) -> String {
        self(value)
    }
}

/// Identity path policy used by generic CLI and CI consumers.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoPathMapping;

impl PathMapper for NoPathMapping {
    fn map_path(&self, value: &str) -> String {
        value.to_owned()
    }
}

/// Stable typed rank key; lower values are retained first.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct RankKey {
    pub severity: Severity,
    pub class: u8,
    pub quality: EvidenceQuality,
    pub ordinal: u64,
}

/// Provider-selectable ranking policy that never inspects raw log state.
pub trait Ranker {
    fn rank(&self, diagnostic: &Diagnostic) -> RankKey;
}

/// Generic ranking prefers compiler, test, lint, infrastructure, then tool
/// diagnostics and uses parser-emitted evidence quality instead of message
/// substring heuristics.
#[derive(Clone, Copy, Debug, Default)]
pub struct GenericRanker;

impl Ranker for GenericRanker {
    fn rank(&self, diagnostic: &Diagnostic) -> RankKey {
        let class = match diagnostic.class {
            DiagnosticClass::Compiler => 0,
            DiagnosticClass::Test => 1,
            DiagnosticClass::Lint => 2,
            DiagnosticClass::Infrastructure => 3,
            DiagnosticClass::Tool => 4,
        };
        RankKey {
            severity: diagnostic.severity,
            class,
            quality: diagnostic.quality,
            ordinal: diagnostic
                .provenance
                .as_ref()
                .and_then(|provenance| provenance.start_line)
                .unwrap_or(u64::MAX),
        }
    }
}

/// Output transformations and ranking shared by batch and streaming callers.
#[derive(Clone, Copy)]
pub struct OutputPolicy<'a> {
    pub redactor: &'a dyn Redactor,
    pub path_mapper: &'a dyn PathMapper,
    pub ranker: &'a dyn Ranker,
}

impl<'a> OutputPolicy<'a> {
    #[must_use]
    pub const fn new(
        redactor: &'a dyn Redactor,
        path_mapper: &'a dyn PathMapper,
        ranker: &'a dyn Ranker,
    ) -> Self {
        Self {
            redactor,
            path_mapper,
            ranker,
        }
    }
}
