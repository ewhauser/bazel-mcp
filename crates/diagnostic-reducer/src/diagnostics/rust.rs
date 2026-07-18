use crate::{Diagnostic, DiagnosticClass, Location, Severity};

use super::common::{bounded_text, normalize_path};

const MAX_MESSAGE_BYTES: usize = 1_024;

pub(super) fn reduce(input: &str, diagnostics: &mut Vec<Diagnostic>) {
    let mut pending = None;
    for line in input.lines() {
        if let Some(message) = parse_error_header(line) {
            if let Some(diagnostic) = pending.take().and_then(PendingDiagnostic::finish) {
                diagnostics.push(diagnostic);
            }
            pending = Some(PendingDiagnostic::new(message));
            continue;
        }
        let Some(diagnostic) = pending.as_mut() else {
            continue;
        };
        if diagnostic.location.is_none()
            && let Some(location) = parse_location(line)
        {
            diagnostic.location = Some(location);
        }
        if diagnostic.detail.is_none()
            && let Some(detail) = parse_detail(line)
        {
            diagnostic.detail = Some(detail.to_owned());
        }
    }
    if let Some(diagnostic) = pending.and_then(PendingDiagnostic::finish) {
        diagnostics.push(diagnostic);
    }
}

#[derive(Debug)]
struct PendingDiagnostic {
    message: String,
    detail: Option<String>,
    location: Option<Location>,
}

impl PendingDiagnostic {
    fn new(message: String) -> Self {
        Self {
            message,
            detail: None,
            location: None,
        }
    }

    fn finish(self) -> Option<Diagnostic> {
        let location = self.location?;
        let message = self.detail.map_or(self.message.clone(), |detail| {
            format!("{}; {detail}", self.message)
        });
        Some(Diagnostic {
            severity: Severity::Error,
            class: DiagnosticClass::Compiler,
            code: None,
            provenance: None,
            message: bounded_text(&message, MAX_MESSAGE_BYTES),
            location: Some(location),
            quality: crate::EvidenceQuality::Located,
            repetition_count: 1,
        })
    }
}

pub(super) fn parse_error_header(line: &str) -> Option<String> {
    let line = line.trim().strip_prefix("ERROR: ").unwrap_or(line.trim());
    let remainder = line.strip_prefix("error[")?;
    let (code, message) = remainder.split_once("]: ")?;
    if !code.strip_prefix('E').is_some_and(|digits| {
        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
    }) {
        return None;
    }
    let message = message.trim();
    (!message.is_empty()).then(|| format!("{code}: {message}"))
}

fn parse_location(line: &str) -> Option<Location> {
    let coordinates = line.trim().strip_prefix("--> ")?;
    let (path_and_line, column) = coordinates.rsplit_once(':')?;
    let (path, line) = path_and_line.rsplit_once(':')?;
    let line = line.parse::<u32>().ok()?;
    let column = column.parse::<u32>().ok()?;
    (!path.is_empty()).then(|| Location {
        path: compact_path(path),
        line: Some(line),
        column: Some(column),
    })
}

fn parse_detail(line: &str) -> Option<&str> {
    let line = line.trim().strip_prefix('|').unwrap_or(line.trim()).trim();
    let expected = line.find("expected `")?;
    let detail = &line[expected..];
    detail.contains("found `").then_some(detail)
}

fn compact_path(path: &str) -> String {
    normalize_path(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_rust_error_code_location_and_type_details() {
        let input = r#"error[E0308]: mismatched types
 --> cases/type_mismatch.rs:2:28
  |
2 |     let invoice_id: &str = &Some("INV-42".to_owned());
  |                     ----   ^^^^^^^^^^^^^^^^^^^^^^^^^^ expected `&str`, found `&Option<String>`
  |
  = note: expected reference `&str`
             found reference `&Option<String>`
error: aborting due to 1 previous error
"#;
        let mut diagnostics = Vec::new();

        reduce(input, &mut diagnostics);

        assert_eq!(diagnostics.len(), 1);
        assert_eq!(
            diagnostics[0].message,
            "E0308: mismatched types; expected `&str`, found `&Option<String>`"
        );
        assert_eq!(
            diagnostics[0].location,
            Some(Location {
                path: "cases/type_mismatch.rs".to_owned(),
                line: Some(2),
                column: Some(28),
            })
        );
    }

    #[test]
    fn ignores_error_codes_without_a_primary_location() {
        let mut diagnostics = Vec::new();

        reduce(
            "error[E9999]: synthetic wrapper without source evidence",
            &mut diagnostics,
        );

        assert!(diagnostics.is_empty());
    }
}
