use bazel_mcp_types::{Diagnostic, DiagnosticCategory, DiagnosticLocation, Severity};

use super::common::{split_u32_prefix, strip_workspace_marker};

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
        category: DiagnosticCategory::Compilation,
        message: message.to_owned(),
        location: Some(DiagnosticLocation {
            path: compact_path(path),
            line: Some(line_number),
            column,
        }),
        target: None,
        action: None,
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
    path.strip_prefix("./").unwrap_or(&path).to_owned()
}
