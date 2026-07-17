use regex::Regex;

use crate::PolicyError;

#[derive(Clone, Debug, Default)]
pub struct Redactor {
    patterns: Vec<Regex>,
}

impl Redactor {
    pub fn new(patterns: &[String]) -> Result<Self, PolicyError> {
        let patterns = patterns
            .iter()
            .map(|pattern| {
                Regex::new(pattern).map_err(|source| PolicyError::InvalidRedaction {
                    pattern: pattern.clone(),
                    source,
                })
            })
            .collect::<Result<_, _>>()?;
        Ok(Self { patterns })
    }

    #[must_use]
    pub fn redact(&self, value: &str) -> String {
        self.redact_bounded(value, value.len().max("[REDACTED]".len()))
    }

    /// Redacts matches without allowing replacement text to grow the result
    /// beyond `maximum_bytes`.
    #[must_use]
    pub fn redact_bounded(&self, value: &str, maximum_bytes: usize) -> String {
        let mut output = String::new();
        self.redact_bounded_into(value, maximum_bytes, &mut output);
        output
    }

    /// Redacts matches into a caller-owned buffer so repeated operations can
    /// reuse its allocation.
    pub fn redact_bounded_into(&self, value: &str, maximum_bytes: usize, output: &mut String) {
        let mut patterns = self.patterns.iter();
        let Some(first) = patterns.next() else {
            output.clear();
            push_bounded(output, value, maximum_bytes);
            return;
        };
        replace_all_bounded(first, value, maximum_bytes, output);
        let mut scratch = String::new();
        for pattern in patterns {
            replace_all_bounded(pattern, output, maximum_bytes, &mut scratch);
            std::mem::swap(output, &mut scratch);
        }
    }
}

fn replace_all_bounded(pattern: &Regex, value: &str, maximum_bytes: usize, output: &mut String) {
    const REPLACEMENT: &str = "[REDACTED]";

    output.clear();
    output.reserve(value.len().min(maximum_bytes));
    let mut previous_end = 0;
    for matched in pattern.find_iter(value) {
        if !push_bounded(output, &value[previous_end..matched.start()], maximum_bytes) {
            return;
        }
        if output.len().saturating_add(REPLACEMENT.len()) > maximum_bytes {
            return;
        }
        output.push_str(REPLACEMENT);
        previous_end = matched.end();
    }
    push_bounded(output, &value[previous_end..], maximum_bytes);
}

fn push_bounded(output: &mut String, value: &str, maximum_bytes: usize) -> bool {
    let remaining = maximum_bytes.saturating_sub(output.len());
    if value.len() <= remaining {
        output.push_str(value);
        return true;
    }
    let mut boundary = remaining;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    output.push_str(&value[..boundary]);
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_every_configured_match() {
        let redactor = Redactor::new(&[r"token=[^\s]+".to_owned()]).unwrap();
        assert_eq!(redactor.redact("x token=secret y"), "x [REDACTED] y");
    }

    #[test]
    fn bounds_adversarial_replacement_expansion() {
        let redactor = Redactor::new(&[".".to_owned()]).unwrap();
        let redacted = redactor.redact_bounded(&"x".repeat(1_000), 128);
        assert_eq!(redacted.len(), 120);
        assert_eq!(redacted, "[REDACTED]".repeat(12));
    }

    #[test]
    fn never_exposes_a_secret_cut_by_the_output_boundary() {
        let redactor = Redactor::new(&[r"token=[^\s]+".to_owned()]).unwrap();
        assert_eq!(redactor.redact_bounded("token=secret", 5), "");
    }

    #[test]
    fn reusable_output_matches_owned_redaction_for_multiple_patterns() {
        let redactor =
            Redactor::new(&[r"token=[^\s]+".to_owned(), r"password=[^\s]+".to_owned()]).unwrap();
        let mut output = String::with_capacity(128);
        redactor.redact_bounded_into("token=secret password=hunter2 visible", 128, &mut output);
        assert_eq!(output, "[REDACTED] [REDACTED] visible");

        redactor.redact_bounded_into("short", 128, &mut output);
        assert_eq!(output, "short");
    }
}
