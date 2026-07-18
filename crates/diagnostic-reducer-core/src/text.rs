use std::collections::BTreeMap;

/// Removes terminal control sequences and normalizes progress redraws to lines.
#[must_use]
pub fn normalize_terminal_text(input: &[u8]) -> String {
    let decoded = String::from_utf8_lossy(input);
    let mut output = String::with_capacity(decoded.len());
    let mut chars = decoded.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '\u{1b}' {
            match chars.peek().copied() {
                Some('[') => {
                    chars.next();
                    for next in chars.by_ref() {
                        if ('@'..='~').contains(&next) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(next) = chars.next() {
                        if next == '\u{7}' {
                            break;
                        }
                        if next == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            }
            continue;
        }
        if character == '\r' {
            output.push('\n');
        } else if character == '\n' || character == '\t' || !character.is_control() {
            output.push(character);
        }
    }

    output
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Exact line deduplication that preserves first-seen order.
#[must_use]
pub fn deduplicate_lines(input: &str) -> Vec<(String, u32)> {
    let mut counts = BTreeMap::<&str, u32>::new();
    let mut order = Vec::new();
    for line in input.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if let Some(count) = counts.get_mut(line) {
            *count = count.saturating_add(1);
        } else {
            order.push(line);
            counts.insert(line, 1);
        }
    }
    order
        .into_iter()
        .map(|line| (line.to_owned(), counts.get(line).copied().unwrap_or(1)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_terminal_controls_and_rewrites_carriage_returns() {
        assert_eq!(
            normalize_terminal_text(b"\x1b[31mERROR\x1b[0m\rpro\x08gress\x00"),
            "ERROR\nprogress"
        );
    }

    #[test]
    fn exact_deduplication_preserves_first_order() {
        assert_eq!(
            deduplicate_lines(" warning \nerror\nwarning\n\n error "),
            vec![("warning".into(), 2), ("error".into(), 2)]
        );
    }
}
