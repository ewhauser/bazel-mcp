use bazel_mcp_types::{Diagnostic, DiagnosticCategory, DiagnosticLocation, Severity};

use super::common::{split_u32_prefix, strip_workspace_marker};

pub(super) fn reduce(input: &str, diagnostics: &mut Vec<Diagnostic>) {
    let mut parser = StarlarkDiagnosticParser::default();
    for line in input.lines() {
        if let Some(diagnostic) = parser.observe_line(line) {
            diagnostics.push(diagnostic);
        }
    }
}

/// Stateful extractor for Bazel's Starlark source and traceback diagnostics.
///
/// Syntax diagnostics carry their location inline. Runtime Starlark failures
/// instead print one or more `File "...", line N, column N` frames before a
/// terminal `Error in ...` line, so only the latest frame must be retained.
#[derive(Debug)]
struct StarlarkDiagnosticParser {
    location: Option<DiagnosticLocation>,
    category: DiagnosticCategory,
}

impl Default for StarlarkDiagnosticParser {
    fn default() -> Self {
        Self {
            location: None,
            category: DiagnosticCategory::Loading,
        }
    }
}

impl StarlarkDiagnosticParser {
    fn observe_line(&mut self, line: &str) -> Option<Diagnostic> {
        if is_traceback_header(line) {
            self.location = None;
            if line.trim_start().starts_with("ERROR:") {
                self.category = DiagnosticCategory::Loading;
            }
            return None;
        }
        if let Some(diagnostic) = parse_inline_diagnostic(line) {
            self.location = diagnostic.location.clone();
            self.category = diagnostic.category;
            return is_root_cause_message(&diagnostic.message).then_some(diagnostic);
        }
        if let Some(location) = parse_traceback_location(line) {
            self.location = Some(location);
            return None;
        }
        let message = error_message(line)?;
        Some(Diagnostic {
            severity: Severity::Error,
            category: self.category,
            message: message.to_owned(),
            location: self.location.take(),
            target: None,
            action: None,
            repetition_count: 1,
        })
    }
}

pub(crate) fn parse_inline_diagnostic(line: &str) -> Option<Diagnostic> {
    let line = line
        .trim()
        .strip_prefix("ERROR: ")
        .unwrap_or_else(|| line.trim());
    let path_end = path_end(line)?;
    let path = &line[..path_end];
    let (line_number, remainder) = split_u32_prefix(&line[path_end + 1..])?;
    let (column, message) = split_u32_prefix(remainder)
        .map_or((None, remainder), |(column, message)| {
            (Some(column), message)
        });
    let message = message.trim();
    if message.is_empty() {
        return None;
    }
    let lower = message.to_ascii_lowercase();
    let category = if (lower.starts_with("in ") && lower.contains(" rule //"))
        || lower.contains("analysis of target")
        || lower.contains("aspect on target")
    {
        DiagnosticCategory::Analysis
    } else {
        DiagnosticCategory::Loading
    };
    Some(Diagnostic {
        severity: if lower.contains("warning:") {
            Severity::Warning
        } else {
            Severity::Error
        },
        category,
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

fn path_end(line: &str) -> Option<usize> {
    const MARKERS: [&str; 6] = [
        ".bzl:",
        ".bazel:",
        "/BUILD:",
        "\\BUILD:",
        "/WORKSPACE:",
        "\\WORKSPACE:",
    ];
    MARKERS
        .iter()
        .filter_map(|marker| {
            line.rfind(marker)
                .map(|index| index + marker.len().saturating_sub(1))
        })
        .max()
}

fn parse_traceback_location(line: &str) -> Option<DiagnosticLocation> {
    let marker = "File \"";
    let start = line.find(marker)? + marker.len();
    let remainder = &line[start..];
    let (path, remainder) = remainder.split_once("\", line ")?;
    if !is_path(path) {
        return None;
    }
    let line_digits = remainder
        .bytes()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if line_digits == 0 {
        return None;
    }
    let line_number = remainder[..line_digits].parse::<u32>().ok()?;
    let column = remainder[line_digits..]
        .strip_prefix(", column ")
        .and_then(|remainder| {
            let digits = remainder
                .bytes()
                .take_while(|byte| byte.is_ascii_digit())
                .count();
            (digits > 0)
                .then(|| remainder[..digits].parse::<u32>().ok())
                .flatten()
        });
    Some(DiagnosticLocation {
        path: compact_path(path),
        line: Some(line_number),
        column,
    })
}

fn is_path(path: &str) -> bool {
    path.ends_with(".bzl")
        || path.ends_with(".bazel")
        || matches!(path.rsplit(['/', '\\']).next(), Some("BUILD" | "WORKSPACE"))
}

pub(super) fn is_traceback_header(line: &str) -> bool {
    line.trim()
        .strip_prefix("ERROR: ")
        .unwrap_or_else(|| line.trim())
        == "Traceback (most recent call last):"
}

pub(super) fn error_message(line: &str) -> Option<&str> {
    let line = line.trim();
    (line.starts_with("Error in ") || line.starts_with("Error: ")).then_some(line)
}

pub(super) fn is_root_cause_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("syntax error")
        || lower.contains("contains syntax errors")
        || (lower.contains("name '") && lower.contains(" is not defined"))
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
