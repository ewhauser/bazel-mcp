use std::collections::BTreeMap;

use crate::{
    Budget, Diagnostic, DiagnosticClass, EvidenceQuality, Location, OutputPolicy, Provenance,
    Reduction, ReductionStats, Severity, Stream, TestFailure, normalize_terminal_text,
};

type DiagnosticKey = (
    Severity,
    DiagnosticClass,
    Option<String>,
    String,
    Option<Location>,
    Option<Provenance>,
);

/// Applies output policy, exact deduplication, typed ranking, and byte budgets.
#[must_use]
pub fn finalize_findings(
    mut diagnostics: Vec<Diagnostic>,
    mut test_failures: Vec<TestFailure>,
    budget: Budget,
    policy: OutputPolicy<'_>,
    mut stats: ReductionStats,
) -> Reduction {
    for diagnostic in &mut diagnostics {
        diagnostic.message = sanitize(policy.redactor, &diagnostic.message, false);
        if let Some(code) = &mut diagnostic.code {
            *code = sanitize(policy.redactor, code, true);
        }
        if let Some(location) = &mut diagnostic.location {
            location.path = sanitize(
                policy.redactor,
                &policy.path_mapper.map_path(&location.path),
                true,
            );
        }
        sanitize_provenance(diagnostic.provenance.as_mut(), policy);
    }
    for failure in &mut test_failures {
        failure.name = sanitize(policy.redactor, &failure.name, true);
        failure.message = sanitize(policy.redactor, &failure.message, false);
        failure.framework = failure
            .framework
            .as_deref()
            .map(|value| sanitize(policy.redactor, value, true));
        if let Some(location) = &mut failure.location {
            location.path = sanitize(
                policy.redactor,
                &policy.path_mapper.map_path(&location.path),
                true,
            );
        }
        sanitize_provenance(failure.provenance.as_mut(), policy);
    }

    suppress_overlapped_fallbacks(&mut diagnostics);

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
            deduplicated[index].quality = deduplicated[index].quality.min(diagnostic.quality);
        } else {
            positions.insert(key, deduplicated.len());
            deduplicated.push(diagnostic);
        }
    }

    deduplicated.sort_by_key(|diagnostic| policy.ranker.rank(diagnostic));
    test_failures.sort_by(|left, right| {
        left.provenance
            .cmp(&right.provenance)
            .then_with(|| left.name.cmp(&right.name))
    });
    test_failures.dedup_by(|left, right| left == right);

    let ranked_diagnostic_count = deduplicated.len();
    let ranked_test_count = test_failures.len();
    let mut remaining_items = budget.max_items;
    if deduplicated.len() > remaining_items {
        deduplicated.truncate(remaining_items);
    }
    remaining_items = remaining_items.saturating_sub(deduplicated.len());
    if test_failures.len() > remaining_items {
        test_failures.truncate(remaining_items);
    }

    let mut used_bytes = 0_usize;
    deduplicated
        .retain(|diagnostic| retain_serialized(diagnostic, budget.max_bytes, &mut used_bytes));
    test_failures.retain(|failure| retain_serialized(failure, budget.max_bytes, &mut used_bytes));

    let omitted_diagnostics = ranked_diagnostic_count.saturating_sub(deduplicated.len());
    stats.omitted_test_failures = ranked_test_count.saturating_sub(test_failures.len());
    let truncated = omitted_diagnostics > 0
        || stats.omitted_test_failures > 0
        || stats.candidates_dropped > 0
        || stats.truncated_lines > 0;

    Reduction {
        diagnostics: deduplicated,
        test_failures,
        truncated,
        omitted_diagnostics,
        used_bytes,
        stats,
    }
}

fn retain_serialized<T: serde::Serialize>(
    value: &T,
    maximum: usize,
    used_bytes: &mut usize,
) -> bool {
    let encoded = serde_json::to_vec(value)
        .expect("serializing a finding containing only infallible data cannot fail")
        .len();
    if used_bytes.saturating_add(encoded) > maximum {
        false
    } else {
        *used_bytes = used_bytes.saturating_add(encoded);
        true
    }
}

fn sanitize_provenance(provenance: Option<&mut Provenance>, policy: OutputPolicy<'_>) {
    let Some(provenance) = provenance else {
        return;
    };
    provenance.source = sanitize(policy.redactor, &provenance.source, true);
    provenance.label = provenance
        .label
        .as_deref()
        .map(|value| sanitize(policy.redactor, value, true));
    provenance.scope_id = provenance
        .scope_id
        .as_deref()
        .map(|value| sanitize(policy.redactor, value, true));
    provenance.parser = provenance
        .parser
        .as_deref()
        .map(|value| sanitize(policy.redactor, value, true));
}

fn sanitize(redactor: &dyn crate::Redactor, value: &str, single_line: bool) -> String {
    let value = redactor.redact(value);
    let value = normalize_terminal_text(value.as_bytes());
    if single_line {
        value.lines().collect::<Vec<_>>().join(" ")
    } else {
        value
    }
}

fn suppress_overlapped_fallbacks(diagnostics: &mut Vec<Diagnostic>) {
    let structured = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.quality != EvidenceQuality::Fallback)
        .filter_map(|diagnostic| diagnostic.provenance.as_ref())
        .filter_map(provenance_span)
        .collect::<Vec<_>>();
    diagnostics.retain(|diagnostic| {
        if diagnostic.quality != EvidenceQuality::Fallback {
            return true;
        }
        let Some(span) = diagnostic.provenance.as_ref().and_then(provenance_span) else {
            return true;
        };
        !structured
            .iter()
            .any(|candidate| spans_overlap(&span, candidate))
    });
}

type ProvenanceSpan = (String, Stream, u64, u64);

fn provenance_span(provenance: &Provenance) -> Option<ProvenanceSpan> {
    Some((
        provenance.scope_id.clone()?,
        provenance.stream?,
        provenance.start_line?,
        provenance.end_line.unwrap_or(provenance.start_line?),
    ))
}

fn spans_overlap(left: &ProvenanceSpan, right: &ProvenanceSpan) -> bool {
    left.0 == right.0 && left.1 == right.1 && left.2 <= right.3 && right.2 <= left.3
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GenericRanker, NoPathMapping, NoRedaction, Scope, ScopeKind};

    fn policy<'a>(
        redactor: &'a dyn crate::Redactor,
        mapper: &'a dyn crate::PathMapper,
        ranker: &'a dyn crate::Ranker,
    ) -> OutputPolicy<'a> {
        OutputPolicy::new(redactor, mapper, ranker)
    }

    #[test]
    fn path_mapping_and_redaction_precede_deduplication() {
        let scope = Scope::new("job", ScopeKind::Job);
        let diagnostic = |path: &str| Diagnostic {
            severity: Severity::Error,
            class: DiagnosticClass::Compiler,
            code: None,
            message: "token=message".to_owned(),
            location: Some(Location {
                path: path.to_owned(),
                line: Some(1),
                column: None,
            }),
            provenance: Some(Provenance::new("stderr").with_scope(&scope)),
            quality: EvidenceQuality::Located,
            repetition_count: 1,
        };
        let redact = |value: &str| value.replace("token=", "[REDACTED]-");
        let mapper = |_: &str| "token=same.rs".to_owned();
        let ranker = GenericRanker;
        let reduction = finalize_findings(
            vec![diagnostic("one.rs"), diagnostic("two.rs")],
            Vec::new(),
            Budget::unbounded(),
            policy(&redact, &mapper, &ranker),
            ReductionStats::default(),
        );
        assert_eq!(reduction.diagnostics.len(), 1);
        assert_eq!(reduction.diagnostics[0].repetition_count, 2);
        assert_eq!(
            reduction.diagnostics[0].location.as_ref().unwrap().path,
            "[REDACTED]-same.rs"
        );
    }

    #[test]
    fn structured_span_suppresses_overlapping_fallback() {
        let provenance = Provenance::new("stderr")
            .with_scope(&Scope::step("compile"))
            .with_span(Stream::Stderr, 4, 5);
        let make = |quality| Diagnostic {
            severity: Severity::Error,
            class: DiagnosticClass::Compiler,
            code: None,
            message: format!("{quality:?}"),
            location: None,
            provenance: Some(provenance.clone()),
            quality,
            repetition_count: 1,
        };
        let reduction = finalize_findings(
            vec![
                make(EvidenceQuality::Fallback),
                make(EvidenceQuality::Structured),
            ],
            Vec::new(),
            Budget::unbounded(),
            OutputPolicy::new(&NoRedaction, &NoPathMapping, &GenericRanker),
            ReductionStats::default(),
        );
        assert_eq!(reduction.diagnostics.len(), 1);
        assert_eq!(
            reduction.diagnostics[0].quality,
            EvidenceQuality::Structured
        );
    }
}
