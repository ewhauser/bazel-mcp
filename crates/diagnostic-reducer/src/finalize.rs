use crate::{Budget, Diagnostic, Reduction};
use diagnostic_reducer_core::{OutputPolicy, ReductionStats, finalize_findings};

pub(crate) fn finalize_with_policy(
    diagnostics: Vec<Diagnostic>,
    budget: Budget,
    policy: OutputPolicy<'_>,
) -> Reduction {
    finalize_findings(
        diagnostics,
        Vec::new(),
        budget,
        policy,
        ReductionStats::default(),
    )
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
