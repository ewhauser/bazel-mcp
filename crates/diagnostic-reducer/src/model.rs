use serde::{Deserialize, Serialize};

/// Model-visible severity assigned by a built-in parser.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
    Note,
}

/// Source-agnostic diagnostic family used for ranking and adapter mapping.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticClass {
    Compiler,
    Test,
    Tool,
}

/// Optional source coordinate extracted from diagnostic text.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct Location {
    pub path: String,
    pub line: Option<u32>,
    pub column: Option<u32>,
}

/// Caller-supplied identity for the text input that emitted a diagnostic.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct Provenance {
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl Provenance {
    #[must_use]
    pub fn new(source: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            label: None,
        }
    }

    #[must_use]
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
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
    pub repetition_count: u32,
}

/// Combined item and serialized-diagnostic byte limits.
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

/// Stable options for one synchronous reduction call.
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

/// Bounded result and explicit truncation accounting.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Reduction {
    pub diagnostics: Vec<Diagnostic>,
    pub truncated: bool,
    pub omitted_diagnostics: usize,
    pub used_bytes: usize,
}
