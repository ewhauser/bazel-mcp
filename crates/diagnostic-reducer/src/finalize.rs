use std::collections::BTreeMap;

use crate::{
    Budget, Diagnostic, DiagnosticClass, Location, Provenance, Redactor, Reduction, Severity,
    normalize_terminal_text,
};

type DiagnosticKey = (
    Severity,
    DiagnosticClass,
    Option<String>,
    String,
    Option<Location>,
    Option<Provenance>,
);

pub(crate) fn finalize(
    mut diagnostics: Vec<Diagnostic>,
    budget: Budget,
    redactor: &dyn Redactor,
) -> Reduction {
    for diagnostic in &mut diagnostics {
        diagnostic.message = sanitize(redactor, &diagnostic.message, false);
        if let Some(location) = &mut diagnostic.location {
            location.path = sanitize(redactor, &location.path, true);
        }
        if let Some(provenance) = &mut diagnostic.provenance {
            provenance.source = sanitize(redactor, &provenance.source, true);
            provenance.label = provenance
                .label
                .as_deref()
                .map(|label| sanitize(redactor, label, true));
        }
    }

    let mut positions = BTreeMap::<DiagnosticKey, usize>::new();
    let mut deduplicated: Vec<Diagnostic> = Vec::with_capacity(diagnostics.len());
    for diagnostic in diagnostics {
        let key = (
            diagnostic.severity,
            diagnostic.class,
            diagnostic.code.clone(),
            diagnostic.message.clone(),
            diagnostic.location.clone(),
            diagnostic.provenance.clone(),
        );
        if let Some(index) = positions.get(&key).copied() {
            deduplicated[index].repetition_count = deduplicated[index]
                .repetition_count
                .saturating_add(diagnostic.repetition_count);
        } else {
            positions.insert(key, deduplicated.len());
            deduplicated.push(diagnostic);
        }
    }

    deduplicated.sort_by_key(diagnostic_priority);
    let ranked_count = deduplicated.len();
    let mut truncated = ranked_count > budget.max_items;
    deduplicated.truncate(budget.max_items);

    let mut used_bytes = 0_usize;
    deduplicated.retain(|diagnostic| {
        let encoded = serde_json::to_vec(diagnostic)
            .expect("serializing a diagnostic containing only infallible data cannot fail")
            .len();
        if used_bytes.saturating_add(encoded) > budget.max_bytes {
            truncated = true;
            false
        } else {
            used_bytes = used_bytes.saturating_add(encoded);
            true
        }
    });

    Reduction {
        omitted_diagnostics: ranked_count.saturating_sub(deduplicated.len()),
        diagnostics: deduplicated,
        truncated,
        used_bytes,
    }
}

fn sanitize(redactor: &dyn Redactor, value: &str, single_line: bool) -> String {
    let value = redactor.redact(value);
    let value = normalize_terminal_text(value.as_bytes());
    if single_line {
        value.lines().collect::<Vec<_>>().join(" ")
    } else {
        value
    }
}

fn diagnostic_priority(diagnostic: &Diagnostic) -> (Severity, u8, u8) {
    let class = match diagnostic.code.as_deref() {
        Some("starlark.loading" | "starlark.analysis" | "tool.loading" | "tool.analysis") => 0,
        _ => match diagnostic.class {
            DiagnosticClass::Compiler => 0,
            DiagnosticClass::Test => 1,
            DiagnosticClass::Tool => 4,
        },
    };
    let message = diagnostic.message.as_str();
    let rust_failure = contains_ignore_ascii_case(message, "panicked at")
        || (contains_ignore_ascii_case(message, "assertion")
            && contains_ignore_ascii_case(message, " failed"));
    let evidence_quality = if diagnostic.location.is_some()
        || (contains_ignore_ascii_case(message, "root_cause")
            && !contains_ignore_ascii_case(message, "error executing"))
    {
        0
    } else if diagnostic.class == DiagnosticClass::Test && rust_failure {
        1
    } else if starts_with_ignore_ascii_case(message, "test failed:")
        || starts_with_ignore_ascii_case(message, "test timed out:")
        || starts_with_ignore_ascii_case(message, "test was incomplete:")
        || starts_with_ignore_ascii_case(message, "test result was unavailable:")
    {
        3
    } else if contains_ignore_ascii_case(message, "error executing") {
        2
    } else {
        1
    };
    (diagnostic.severity, class, evidence_quality)
}

fn contains_ignore_ascii_case(value: &str, needle: &str) -> bool {
    let needle = needle.as_bytes();
    needle.is_empty()
        || value
            .as_bytes()
            .windows(needle.len())
            .any(|window| window.eq_ignore_ascii_case(needle))
}

fn starts_with_ignore_ascii_case(value: &str, prefix: &str) -> bool {
    value
        .as_bytes()
        .get(..prefix.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(prefix.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NoRedaction, ReductionOptions, TextInput, reduce};

    #[test]
    fn redaction_precedes_deduplication_and_budgeting() {
        let inputs = [TextInput::new(
            b"src/a.rs:1:1: error: token=one\nsrc/a.rs:1:1: error: token=two",
        )];
        let redact = |value: &str| {
            if value.contains("token=") {
                "same redacted error".to_owned()
            } else {
                value.to_owned()
            }
        };
        let reduction = reduce(
            &inputs,
            &ReductionOptions {
                budget: Budget::unbounded(),
                ..ReductionOptions::default()
            },
            &redact,
        );
        assert_eq!(reduction.diagnostics.len(), 1);
        assert_eq!(reduction.diagnostics[0].repetition_count, 2);
        assert_eq!(reduction.diagnostics[0].message, "same redacted error");
    }

    #[test]
    fn serialized_byte_budget_is_deterministic() {
        let inputs = [TextInput::new(b"a.go:1:1: first\nb.go:2:1: second")];
        let unbounded = reduce(
            &inputs,
            &ReductionOptions {
                budget: Budget::unbounded(),
                ..ReductionOptions::default()
            },
            &NoRedaction,
        );
        let first_size = serde_json::to_vec(&unbounded.diagnostics[0]).unwrap().len();
        let bounded = reduce(
            &inputs,
            &ReductionOptions {
                budget: Budget {
                    max_bytes: first_size,
                    max_items: 10,
                },
                ..ReductionOptions::default()
            },
            &NoRedaction,
        );
        assert_eq!(bounded.diagnostics.len(), 1);
        assert!(bounded.truncated);
        assert_eq!(bounded.omitted_diagnostics, 1);
        assert_eq!(bounded.used_bytes, first_size);
    }
}
