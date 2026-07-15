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
        let mut patterns = self.patterns.iter();
        let Some(first) = patterns.next() else {
            return bounded_prefix(value, maximum_bytes);
        };
        patterns.fold(
            replace_all_bounded(first, value, maximum_bytes),
            |current, pattern| replace_all_bounded(pattern, &current, maximum_bytes),
        )
    }
}

fn replace_all_bounded(pattern: &Regex, value: &str, maximum_bytes: usize) -> String {
    const REPLACEMENT: &str = "[REDACTED]";

    let mut output = String::with_capacity(value.len().min(maximum_bytes));
    let mut previous_end = 0;
    for matched in pattern.find_iter(value) {
        if !push_bounded(
            &mut output,
            &value[previous_end..matched.start()],
            maximum_bytes,
        ) {
            return output;
        }
        if output.len().saturating_add(REPLACEMENT.len()) > maximum_bytes {
            return output;
        }
        output.push_str(REPLACEMENT);
        previous_end = matched.end();
    }
    push_bounded(&mut output, &value[previous_end..], maximum_bytes);
    output
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

fn bounded_prefix(value: &str, maximum_bytes: usize) -> String {
    let mut output = String::new();
    push_bounded(&mut output, value, maximum_bytes);
    output
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
}
