use crate::{Diagnostic, DiagnosticClass, Location, Severity};

use super::common::{normalize_path, split_u32_prefix};

pub(crate) fn parse_diagnostic(line: &str) -> Option<Diagnostic> {
    let marker = line.rfind(".proto:")?;
    let path_end = marker + ".proto".len();
    let path = line[..path_end]
        .trim()
        .strip_prefix("ERROR: ")
        .unwrap_or_else(|| line[..path_end].trim());
    let (line_number, remainder) = split_u32_prefix(&line[path_end + 1..])?;
    let (column, message) = split_u32_prefix(remainder)
        .map_or((None, remainder), |(column, message)| {
            (Some(column), message)
        });
    let message = message.trim();
    let (severity, message) = if let Some(message) = message.strip_prefix("warning:") {
        (Severity::Warning, message.trim())
    } else if let Some(message) = message.strip_prefix("error:") {
        (Severity::Error, message.trim())
    } else {
        (Severity::Error, message)
    };
    if message.is_empty() {
        return None;
    }
    Some(Diagnostic {
        severity,
        class: DiagnosticClass::Compiler,
        code: None,
        provenance: None,
        message: message.to_owned(),
        location: Some(Location {
            path: compact_path(path),
            line: Some(line_number),
            column,
        }),
        quality: crate::EvidenceQuality::Located,
        repetition_count: 1,
    })
}

fn compact_path(path: &str) -> String {
    normalize_path(path)
}
