use crate::{Diagnostic, DiagnosticClass, Location, Severity};

use super::common::{split_u32_prefix, strip_workspace_marker};

/// Parses the standard Go compiler location form without depending on a
/// particular diagnostic message or language setting.
#[must_use]
pub fn parse_diagnostic(line: &str) -> Option<Diagnostic> {
    let marker = line.rfind(".go:")?;
    let path_end = marker + ".go".len();
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
    if message.is_empty() {
        return None;
    }
    Some(Diagnostic {
        severity: if message.to_ascii_lowercase().contains("warning:") {
            Severity::Warning
        } else {
            Severity::Error
        },
        class: DiagnosticClass::Compiler,
        code: None,
        provenance: None,
        message: message.to_owned(),
        location: Some(Location {
            path: compact_path(path),
            line: Some(line_number),
            column,
        }),
        repetition_count: 1,
    })
}

pub(super) fn strict_dependency_diagnostic(line: &str) -> Option<Diagnostic> {
    const MARKER: &str = ": import of \"";
    let marker = line.find(MARKER)?;
    let path = line[..marker].trim();
    if !path.ends_with(".go") {
        return None;
    }
    let import = line[marker + MARKER.len()..].split('"').next()?.trim();
    if import.is_empty() {
        return None;
    }
    let path = compact_path(path);
    Some(Diagnostic {
        severity: Severity::Error,
        class: DiagnosticClass::Compiler,
        code: None,
        provenance: None,
        message: format!(
            "missing strict dependency: {path} imports \"{import}\"; add its target to deps"
        ),
        location: Some(Location {
            path,
            line: None,
            column: None,
        }),
        repetition_count: 1,
    })
}

fn compact_path(path: &str) -> String {
    let path = strip_workspace_marker(path.trim_matches('"').replace('\\', "/"));
    if let Some((_, after_execroot)) = path.rsplit_once("/execroot/")
        && let Some((_, relative)) = after_execroot.split_once('/')
    {
        return relative.to_owned();
    }
    path
}
