use std::{cmp::Reverse, collections::BTreeSet, sync::Arc};

use bazel_mcp_types::{Diagnostic, InspectHint, InvocationSummary};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReducerMode {
    Augment,
    OverrideMatching,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReducerEventKind {
    Aborted,
    Action,
    Target,
    TestSummary,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReducerEvent {
    pub(crate) ordinal: u64,
    pub(crate) kind: ReducerEventKind,
    pub label: Option<String>,
    pub target_kind: Option<String>,
    pub action_type: Option<String>,
    pub(crate) success: Option<bool>,
    pub(crate) exit_code: Option<i32>,
    pub message: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ReducerContext {
    pub api_version: u32,
    pub command: String,
    pub arguments: Vec<String>,
    pub exit_code: Option<i32>,
    pub elapsed_ms: u64,
    pub stdout: String,
    pub stderr: String,
    pub events: Vec<ReducerEvent>,
    pub input_truncated: bool,
    pub baseline: InvocationSummary,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ReducerSelector {
    pub commands: BTreeSet<String>,
    pub target_labels: Vec<String>,
    pub target_kinds: BTreeSet<String>,
    pub action_types: BTreeSet<String>,
}

impl ReducerSelector {
    #[must_use]
    fn matches(&self, context: &ReducerContext) -> bool {
        if !self.commands.is_empty() && !self.commands.contains(&context.command) {
            return false;
        }
        if !self.has_event_constraints() {
            return true;
        }
        context.events.iter().any(|event| self.matches_event(event))
    }

    #[must_use]
    pub(crate) fn has_event_constraints(&self) -> bool {
        !self.target_labels.is_empty()
            || !self.target_kinds.is_empty()
            || !self.action_types.is_empty()
    }

    fn matches_event(&self, event: &ReducerEvent) -> bool {
        event.label.as_deref().is_some_and(|label| {
            self.target_labels
                .iter()
                .any(|pattern| label_matches(pattern, label))
        }) || event
            .target_kind
            .as_ref()
            .is_some_and(|kind| self.target_kinds.contains(kind))
            || event
                .action_type
                .as_ref()
                .is_some_and(|action| self.action_types.contains(action))
    }

    fn matched_labels(&self, context: &ReducerContext) -> BTreeSet<String> {
        context
            .events
            .iter()
            .filter(|event| self.matches_event(event))
            .filter_map(|event| event.label.clone())
            .collect()
    }
}

fn label_matches(pattern: &str, label: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix("...") {
        return label.starts_with(prefix);
    }
    wildcard_match(pattern.as_bytes(), label.as_bytes())
}

fn wildcard_match(pattern: &[u8], value: &[u8]) -> bool {
    let (mut pattern_index, mut value_index) = (0, 0);
    let (mut star_index, mut star_value_index) = (None, 0);
    while value_index < value.len() {
        if pattern_index < pattern.len() && pattern[pattern_index] == value[value_index] {
            pattern_index += 1;
            value_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
            star_index = Some(pattern_index);
            pattern_index += 1;
            star_value_index = value_index;
        } else if let Some(star) = star_index {
            pattern_index = star + 1;
            star_value_index += 1;
            value_index = star_value_index;
        } else {
            return false;
        }
    }
    while pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReducerPatch {
    #[serde(default)]
    pub headline: Option<String>,
    #[serde(default)]
    pub diagnostics: Vec<Diagnostic>,
    #[serde(default)]
    pub suppress_builtin_diagnostics: bool,
}

impl ReducerPatch {
    #[must_use]
    fn is_empty(&self) -> bool {
        self.headline.is_none() && self.diagnostics.is_empty() && !self.suppress_builtin_diagnostics
    }
}

#[derive(Clone, Debug, Error)]
#[error("{message}")]
pub struct ReducerError {
    message: String,
}

impl ReducerError {
    #[must_use]
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

pub trait CustomReducer: Send + Sync {
    fn name(&self) -> &str;
    fn priority(&self) -> i32;
    fn mode(&self) -> ReducerMode;
    fn selector(&self) -> &ReducerSelector;
    fn reduce(&self, context: &ReducerContext) -> Result<ReducerPatch, ReducerError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReducerFailure {
    pub name: String,
    pub error: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ReducerApplyReport {
    pub applied: Vec<String>,
    pub failures: Vec<ReducerFailure>,
    pub override_collisions: Vec<String>,
    pub headline_applied: bool,
}

#[derive(Clone, Default)]
pub struct ReducerPipeline {
    reducers: Arc<Vec<Arc<dyn CustomReducer>>>,
}

impl ReducerPipeline {
    pub fn new(mut reducers: Vec<Arc<dyn CustomReducer>>) -> Result<Self, ReducerError> {
        let mut names = BTreeSet::new();
        for reducer in &reducers {
            if !names.insert(reducer.name().to_owned()) {
                return Err(ReducerError::new(format!(
                    "duplicate custom reducer name {:?}",
                    reducer.name()
                )));
            }
        }
        reducers.sort_by_key(|reducer| (Reverse(reducer.priority()), reducer.name().to_owned()));
        Ok(Self {
            reducers: Arc::new(reducers),
        })
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.reducers.is_empty()
    }

    pub fn apply(
        &self,
        context: &ReducerContext,
        summary: &mut InvocationSummary,
    ) -> ReducerApplyReport {
        let mut report = ReducerApplyReport::default();
        let mut override_owner = None::<String>;
        let mut headline_set = false;
        for reducer in self
            .reducers
            .iter()
            .filter(|reducer| reducer.selector().matches(context))
        {
            if reducer.mode() == ReducerMode::OverrideMatching
                && let Some(owner) = &override_owner
            {
                report.override_collisions.push(format!(
                    "custom reducer {:?} also matched evidence owned by {:?}; higher priority reducer retained ownership",
                    reducer.name(), owner
                ));
                continue;
            }
            match reducer.reduce(context) {
                Ok(patch) => {
                    if patch.is_empty() {
                        continue;
                    }
                    if reducer.mode() == ReducerMode::OverrideMatching {
                        override_owner = Some(reducer.name().to_owned());
                        if patch.suppress_builtin_diagnostics {
                            suppress_matching_diagnostics(summary, reducer.selector(), context);
                        }
                    }
                    if !headline_set && let Some(headline) = patch.headline {
                        summary.headline = headline;
                        headline_set = true;
                        report.headline_applied = true;
                    }
                    summary.diagnostics.extend(patch.diagnostics);
                    report.applied.push(reducer.name().to_owned());
                }
                Err(error) => report.failures.push(ReducerFailure {
                    name: reducer.name().to_owned(),
                    error: error.to_string(),
                }),
            }
        }
        if !report.applied.is_empty() && context.input_truncated {
            summary.truncated = true;
            summary.inspect_hint = Some(InspectHint::Log);
        }
        report
    }
}

fn suppress_matching_diagnostics(
    summary: &mut InvocationSummary,
    selector: &ReducerSelector,
    context: &ReducerContext,
) {
    let labels = selector.matched_labels(context);
    summary.diagnostics.retain(|diagnostic| {
        let target_matches = diagnostic.target.as_ref().is_some_and(|target| {
            labels.contains(target)
                || selector
                    .target_labels
                    .iter()
                    .any(|pattern| label_matches(pattern, target))
        });
        let action_matches = diagnostic
            .action
            .as_ref()
            .is_some_and(|action| selector.action_types.contains(action));
        !target_matches && !action_matches
    });
}

#[cfg(test)]
mod tests {
    use bazel_mcp_types::{DiagnosticCategory, Severity};

    use super::*;

    #[test]
    fn bazel_label_patterns_are_deterministic() {
        assert!(label_matches("//app/...", "//app/lib:core"));
        assert!(label_matches("//app/*:test", "//app/lib:test"));
        assert!(!label_matches("//app/*:test", "//other/lib:test"));
    }

    #[test]
    fn override_suppression_is_scoped_to_matching_evidence() {
        let selector = ReducerSelector {
            target_kinds: ["swift_library rule".to_owned()].into_iter().collect(),
            ..ReducerSelector::default()
        };
        let context = ReducerContext {
            api_version: 1,
            command: "build".to_owned(),
            arguments: Vec::new(),
            exit_code: Some(1),
            elapsed_ms: 1,
            stdout: String::new(),
            stderr: String::new(),
            events: vec![ReducerEvent {
                ordinal: 0,
                kind: ReducerEventKind::Target,
                label: Some("//swift:lib".to_owned()),
                target_kind: Some("swift_library rule".to_owned()),
                action_type: None,
                success: Some(false),
                exit_code: None,
                message: None,
            }],
            input_truncated: false,
            baseline: InvocationSummary::default(),
        };
        let diagnostic = |target: &str| Diagnostic {
            severity: Severity::Error,
            category: DiagnosticCategory::Compilation,
            message: "failed".to_owned(),
            location: None,
            target: Some(target.to_owned()),
            action: None,
            repetition_count: 1,
        };
        let mut summary = InvocationSummary {
            diagnostics: vec![diagnostic("//swift:lib"), diagnostic("//rust:lib")],
            ..InvocationSummary::default()
        };
        suppress_matching_diagnostics(&mut summary, &selector, &context);
        assert_eq!(summary.diagnostics, vec![diagnostic("//rust:lib")]);
    }
}
