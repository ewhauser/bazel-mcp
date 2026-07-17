use std::sync::OnceLock;

use anyhow::{Result, bail};
use regex::bytes::Regex;

pub fn sanitize_text(input: &[u8], replacements: &[(&str, &[u8])]) -> Result<Vec<u8>> {
    let mut output = input.to_vec();
    let mut replacements = replacements.to_vec();
    replacements.sort_by_key(|(_, source)| std::cmp::Reverse(source.len()));
    for (label, source) in replacements {
        if source.is_empty() {
            continue;
        }
        output = replace_all(&output, source, format!("<{label}>").as_bytes());
    }
    output = normalize_rust_thread_ids(&output, false)?;
    let text = String::from_utf8_lossy(&output);
    let mut normalized = String::with_capacity(text.len());
    for line in text.lines() {
        normalized.push_str(line.trim_end());
        normalized.push('\n');
    }
    while normalized.ends_with("\n\n") {
        normalized.pop();
    }
    let output = normalized.into_bytes();
    reject_forbidden(&output)?;
    Ok(output)
}

pub fn sanitize_binary(input: &[u8], replacements: &[(&str, &[u8])]) -> Result<Vec<u8>> {
    let mut output = input.to_vec();
    let mut replacements = replacements.to_vec();
    replacements.sort_by_key(|(_, source)| std::cmp::Reverse(source.len()));
    for (label, source) in replacements {
        if source.is_empty() {
            continue;
        }
        let replacement = fixed_placeholder(label, source.len())?;
        output = replace_all(&output, source, &replacement);
    }
    output = normalize_rust_thread_ids(&output, true)?;
    reject_forbidden(&output)?;
    Ok(output)
}

pub fn verify_sanitized_evidence(input: &[u8]) -> Result<()> {
    reject_forbidden(input)
}

fn normalize_rust_thread_ids(input: &[u8], preserve_length: bool) -> Result<Vec<u8>> {
    let regex = Regex::new(r"\([0-9]{3,}\) panicked").expect("valid Rust thread ID regex");
    let mut output = Vec::with_capacity(input.len());
    let mut position = 0;
    for found in regex.find_iter(input) {
        output.extend_from_slice(&input[position..found.start()]);
        let identifier_length = found.as_bytes().len() - b" panicked".len();
        if preserve_length {
            output.extend_from_slice(&fixed_placeholder("PID", identifier_length)?);
        } else {
            output.extend_from_slice(b"<PID>");
        }
        output.extend_from_slice(b" panicked");
        position = found.end();
    }
    output.extend_from_slice(&input[position..]);
    Ok(output)
}

fn fixed_placeholder(label: &str, length: usize) -> Result<Vec<u8>> {
    let prefix = format!("<{label}>").into_bytes();
    if prefix.len() > length {
        bail!("placeholder {prefix:?} is longer than its source ({length})");
    }
    let mut output = prefix;
    output.resize(length, b'_');
    Ok(output)
}

fn replace_all(input: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
    if needle.is_empty() {
        return input.to_vec();
    }
    let mut output = Vec::with_capacity(input.len());
    let mut position = 0;
    while let Some(relative) = input[position..]
        .windows(needle.len())
        .position(|window| window == needle)
    {
        let index = position + relative;
        output.extend_from_slice(&input[position..index]);
        output.extend_from_slice(replacement);
        position = index + needle.len();
    }
    output.extend_from_slice(&input[position..]);
    output
}

fn reject_forbidden(output: &[u8]) -> Result<()> {
    for regex in forbidden_patterns() {
        if let Some(found) = regex.find(output) {
            bail!(
                "sanitized evidence still contains forbidden material matching {:?}: {:?}",
                regex.as_str(),
                String::from_utf8_lossy(found.as_bytes())
            );
        }
    }
    Ok(())
}

fn forbidden_patterns() -> &'static [Regex] {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            r"/Users/[^/\x00\s]+",
            r"/home/[^/\x00\s]+",
            r"(?i)[A-Z]:\\Users\\[^\\\x00\s]+",
            r"(?i)token=[^\x00\s]+",
            r"(?i)(api[_-]?key|password|secret)=[^\x00\s]+",
            r"(?i)(authorization|x-buildbuddy-api-key)[=:][ \t]*(basic|bearer)?[ \t]*[^\x00\s]+",
            r"(?i)[?&](x-amz-signature|x-goog-signature)=[^&\x00\s]+",
            r"AKIA[0-9A-Z]{16}",
            r"gh[pousr]_[A-Za-z0-9]{20,}",
            r"-----BEGIN [A-Z ]*PRIVATE KEY-----",
            r"SECRET_SENTINEL",
        ]
        .into_iter()
        .map(|pattern| Regex::new(pattern).expect("valid forbidden fixture regex"))
        .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_sanitization_preserves_wire_length() {
        let input = b"prefix /tmp/private/workspace suffix";
        let output = sanitize_binary(input, &[("WORKSPACE", b"/tmp/private/workspace")]).unwrap();
        assert_eq!(output.len(), input.len());
        assert!(!output.windows(8).any(|window| window == b"private/"));
    }

    #[test]
    fn text_sanitization_rejects_unredacted_credentials() {
        let error = sanitize_text(b"token=still-secret\n", &[]).unwrap_err();
        assert!(error.to_string().contains("forbidden"));
    }

    #[test]
    fn verification_rejects_platform_paths_and_token_families() {
        for forbidden in [
            b"C:\\Users\\alice\\repo".as_slice(),
            b"AKIAIOSFODNN7EXAMPLE".as_slice(),
            b"ghp_abcdefghijklmnopqrstuvwxyz".as_slice(),
            b"-----BEGIN PRIVATE KEY-----".as_slice(),
            b"SECRET_SENTINEL".as_slice(),
        ] {
            assert!(verify_sanitized_evidence(forbidden).is_err());
        }
    }

    #[test]
    fn canonicalizes_libtest_thread_ids_without_breaking_binary_lengths() {
        let input = b"thread 'invoice' (5734767) panicked at cases/test.rs:4:5:";
        let text = sanitize_text(input, &[]).unwrap();
        assert!(String::from_utf8(text).unwrap().contains("<PID> panicked"));
        let binary = sanitize_binary(input, &[]).unwrap();
        assert_eq!(binary.len(), input.len());
        assert!(!binary.windows(7).any(|value| value == b"5734767"));
    }
}
