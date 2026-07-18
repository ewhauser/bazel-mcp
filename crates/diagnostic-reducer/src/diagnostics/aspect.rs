use crate::{Diagnostic, DiagnosticClass, Location, Severity};

/// Parse the stable, one-line diagnostic format emitted by `aspect lint`.
///
/// Example: `🚨 src/app.ts:8:3 · eslint · no-console — unexpected console statement`
#[must_use]
pub(super) fn parse_diagnostic(line: &str) -> Option<Diagnostic> {
    let line = line.trim();
    let (severity, remainder) = if let Some(remainder) = line.strip_prefix("🚨 ") {
        (Severity::Error, remainder)
    } else if let Some(remainder) = line.strip_prefix("⚠️ ").or_else(|| line.strip_prefix("⚠ "))
    {
        (Severity::Warning, remainder)
    } else {
        let remainder = line
            .strip_prefix("ℹ️ ")
            .or_else(|| line.strip_prefix("ℹ "))?;
        (Severity::Note, remainder)
    };
    let (fields, message) = remainder.split_once(" — ")?;
    let mut fields = fields.split(" · ").map(str::trim);
    let location = parse_location(fields.next()?)?;
    let tool = fields.next().filter(|value| !value.is_empty());
    let rule = fields.next().filter(|value| !value.is_empty());
    let message = message.trim();
    if message.is_empty() {
        return None;
    }
    let message = match (tool, rule) {
        (Some(tool), Some(rule)) => format!("{tool} [{rule}]: {message}"),
        (Some(tool), None) => format!("{tool}: {message}"),
        (None, Some(rule)) => format!("[{rule}]: {message}"),
        (None, None) => message.to_owned(),
    };
    Some(Diagnostic {
        severity,
        class: DiagnosticClass::Compiler,
        code: rule.map(str::to_owned),
        message,
        location: Some(location),
        provenance: None,
        repetition_count: 1,
    })
}

fn parse_location(value: &str) -> Option<Location> {
    let (prefix, final_number) = value.trim().rsplit_once(':')?;
    let final_number = final_number.parse::<u32>().ok()?;
    if let Some((path, line)) = prefix.rsplit_once(':')
        && let Ok(line) = line.parse::<u32>()
        && !path.is_empty()
    {
        return Some(Location {
            path: path.to_owned(),
            line: Some(line),
            column: Some(final_number),
        });
    }
    (!prefix.is_empty()).then(|| Location {
        path: prefix.to_owned(),
        line: Some(final_number),
        column: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_aspect_lint_error_with_tool_rule_and_location() {
        let diagnostic = parse_diagnostic(
            "🚨 src/app.ts:8:3 · eslint · no-console — unexpected console statement",
        )
        .unwrap();

        assert_eq!(diagnostic.severity, Severity::Error);
        assert_eq!(diagnostic.code.as_deref(), Some("no-console"));
        assert_eq!(
            diagnostic.message,
            "eslint [no-console]: unexpected console statement"
        );
        assert_eq!(
            diagnostic.location,
            Some(Location {
                path: "src/app.ts".to_owned(),
                line: Some(8),
                column: Some(3),
            })
        );
    }

    #[test]
    fn parses_warning_with_a_line_only_location_and_rejects_unmarked_text() {
        let diagnostic =
            parse_diagnostic("⚠️ scripts/check.sh:12 · shellcheck · SC2086 — quote this value")
                .unwrap();

        assert_eq!(diagnostic.severity, Severity::Warning);
        assert_eq!(diagnostic.location.unwrap().column, None);
        assert!(parse_diagnostic("scripts/check.sh:12: ordinary output").is_none());
    }
}
