use bazel_mcp_types::{Diagnostic, DiagnosticCategory, DiagnosticLocation, Severity};
use logcompact_builtins::{PathMapper, deduplicate_lines};

use super::bounded_text;

/// Bazel-owned path projection applied after generic parsing and before output
/// redaction, deduplication, and byte accounting.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct BazelPathMapper;

impl PathMapper for BazelPathMapper {
    fn map_path(&self, value: &str) -> String {
        compact_bazel_path(value)
    }
}

pub(crate) fn add_bazel_diagnostics(input: &str, diagnostics: &mut Vec<Diagnostic>) {
    let mut starlark = StarlarkDiagnosticParser::default();
    let mut has_inline_starlark = false;
    let mut inline_starlark_category = DiagnosticCategory::Loading;
    for diagnostic in input.lines().filter_map(parse_starlark_inline) {
        has_inline_starlark = true;
        if diagnostic.category == DiagnosticCategory::Analysis {
            inline_starlark_category = DiagnosticCategory::Analysis;
        }
    }
    let has_starlark_context = has_inline_starlark
        || input.lines().any(|line| {
            is_starlark_traceback_header(line) || parse_starlark_traceback_location(line).is_some()
        });
    let starlark_category = has_starlark_context.then_some(inline_starlark_category);
    for line in input.lines() {
        if let Some(diagnostic) = starlark.observe_line(line) {
            diagnostics.push(diagnostic);
        }
    }

    for (line, repetition_count) in deduplicate_lines(input) {
        if is_starlark_traceback_header(&line)
            || parse_starlark_traceback_location(&line).is_some()
            || starlark_error_message(&line).is_some()
        {
            continue;
        }
        let parsed = parse_aspect_lint(&line)
            .or_else(|| parse_strict_dependency(&line))
            .or_else(|| parse_starlark_wrapper(&line))
            .or_else(|| {
                starlark_category
                    .and_then(|category| parse_starlark_source_context(&line, category))
            })
            .or_else(|| parse_bazel_fallback(&line));
        if let Some(mut diagnostic) = parsed {
            diagnostic.repetition_count = repetition_count;
            diagnostics.push(diagnostic);
        }
    }
}

fn parse_starlark_source_context(line: &str, category: DiagnosticCategory) -> Option<Diagnostic> {
    let line = line.trim();
    let lower = line.to_ascii_lowercase();
    if !lower.contains("root_cause")
        || lower.contains("error:")
        || lower.contains("error[")
        || (lower.starts_with("test ") && lower.ends_with(" ... ok"))
    {
        return None;
    }
    Some(Diagnostic {
        severity: Severity::Error,
        category,
        message: line.to_owned(),
        location: None,
        target: None,
        action: None,
        repetition_count: 1,
    })
}

fn parse_starlark_wrapper(line: &str) -> Option<Diagnostic> {
    let lower = line.to_ascii_lowercase();
    let mut diagnostic = parse_starlark_inline(line)?;
    if is_starlark_root_cause(&diagnostic.message) {
        return None;
    }
    diagnostic.category = if lower.contains("visibility") {
        DiagnosticCategory::Visibility
    } else if lower.contains("analysis") {
        DiagnosticCategory::Analysis
    } else {
        DiagnosticCategory::Compilation
    };
    diagnostic.location = None;
    diagnostic.message = line.trim().to_owned();
    Some(diagnostic)
}

pub(crate) fn is_bazel_owned_line(line: &str) -> bool {
    is_bazel_status_line(line)
        || parse_aspect_lint(line).is_some()
        || parse_strict_dependency(line).is_some()
        || parse_bazel_fallback(line).is_some()
        || parse_starlark_inline(line).is_some()
        || parse_starlark_traceback_location(line).is_some()
        || is_starlark_traceback_header(line)
        || starlark_error_message(line).is_some()
}

fn is_bazel_status_line(line: &str) -> bool {
    let lower = line.trim().to_ascii_lowercase();
    lower.starts_with("debug:")
        || lower.starts_with("info:")
        || lower == "error: build did not complete successfully"
}

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
        if is_starlark_traceback_header(line) {
            self.location = None;
            if line.trim_start().starts_with("ERROR:") {
                self.category = DiagnosticCategory::Loading;
            }
            return None;
        }
        if let Some(diagnostic) = parse_starlark_inline(line) {
            self.location = diagnostic.location.clone();
            self.category = diagnostic.category;
            return is_starlark_root_cause(&diagnostic.message).then_some(diagnostic);
        }
        if let Some(location) = parse_starlark_traceback_location(line) {
            self.location = Some(location);
            return None;
        }
        let message = starlark_error_message(line)?;
        Some(Diagnostic {
            severity: Severity::Error,
            category: self.category,
            message: bounded_text(message, 64 * 1024),
            location: self.location.take(),
            target: None,
            action: None,
            repetition_count: 1,
        })
    }
}

pub(crate) fn parse_starlark_inline(line: &str) -> Option<Diagnostic> {
    let line = line.trim();
    if line.starts_with("DEBUG: ") || line.starts_with("INFO: ") {
        return None;
    }
    let line = line.strip_prefix("ERROR: ").unwrap_or(line);
    let path_end = starlark_path_end(line)?;
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
            path: compact_bazel_path(path),
            line: Some(line_number),
            column,
        }),
        target: None,
        action: None,
        repetition_count: 1,
    })
}

fn starlark_path_end(line: &str) -> Option<usize> {
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

fn parse_starlark_traceback_location(line: &str) -> Option<DiagnosticLocation> {
    let marker = "File \"";
    let start = line.find(marker)? + marker.len();
    let remainder = &line[start..];
    let (path, remainder) = remainder.split_once("\", line ")?;
    if !is_starlark_path(path) {
        return None;
    }
    let line_digits = remainder.bytes().take_while(u8::is_ascii_digit).count();
    if line_digits == 0 {
        return None;
    }
    let line_number = remainder[..line_digits].parse::<u32>().ok()?;
    let column = remainder[line_digits..]
        .strip_prefix(", column ")
        .and_then(|remainder| {
            let digits = remainder.bytes().take_while(u8::is_ascii_digit).count();
            (digits > 0)
                .then(|| remainder[..digits].parse::<u32>().ok())
                .flatten()
        });
    Some(DiagnosticLocation {
        path: compact_bazel_path(path),
        line: Some(line_number),
        column,
    })
}

fn is_starlark_path(path: &str) -> bool {
    path.ends_with(".bzl")
        || path.ends_with(".bazel")
        || matches!(path.rsplit(['/', '\\']).next(), Some("BUILD" | "WORKSPACE"))
}

fn is_starlark_traceback_header(line: &str) -> bool {
    line.trim()
        .strip_prefix("ERROR: ")
        .unwrap_or_else(|| line.trim())
        == "Traceback (most recent call last):"
}

fn starlark_error_message(line: &str) -> Option<&str> {
    let line = line.trim();
    (line.starts_with("Error in ") || line.starts_with("Error: ")).then_some(line)
}

fn is_starlark_root_cause(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("syntax error")
        || lower.contains("contains syntax errors")
        || (lower.contains("name '") && lower.contains(" is not defined"))
}

fn parse_aspect_lint(line: &str) -> Option<Diagnostic> {
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
    let location = parse_aspect_location(fields.next()?)?;
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
        category: DiagnosticCategory::Compilation,
        message,
        location: Some(location),
        target: None,
        action: None,
        repetition_count: 1,
    })
}

fn parse_aspect_location(value: &str) -> Option<DiagnosticLocation> {
    let (prefix, final_number) = value.trim().rsplit_once(':')?;
    let final_number = final_number.parse::<u32>().ok()?;
    if let Some((path, line)) = prefix.rsplit_once(':')
        && let Ok(line) = line.parse::<u32>()
        && !path.is_empty()
    {
        return Some(DiagnosticLocation {
            path: compact_bazel_path(path),
            line: Some(line),
            column: Some(final_number),
        });
    }
    (!prefix.is_empty()).then(|| DiagnosticLocation {
        path: compact_bazel_path(prefix),
        line: Some(final_number),
        column: None,
    })
}

fn parse_strict_dependency(line: &str) -> Option<Diagnostic> {
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
    let path = compact_bazel_path(path);
    Some(Diagnostic {
        severity: Severity::Error,
        category: DiagnosticCategory::Compilation,
        message: format!(
            "missing strict dependency: {path} imports \"{import}\"; add its target to deps"
        ),
        location: Some(DiagnosticLocation {
            path,
            line: None,
            column: None,
        }),
        target: None,
        action: None,
        repetition_count: 1,
    })
}

fn parse_bazel_fallback(line: &str) -> Option<Diagnostic> {
    let line = line.trim();
    let lower = line.to_ascii_lowercase();
    if matches!(lower.as_str(), "failure:" | "failures:")
        || (lower.starts_with("test ") && lower.ends_with(" ... ok"))
    {
        return None;
    }
    let category =
        if lower.contains("root_cause") && !lower.contains("error:") && !lower.contains("error[") {
            DiagnosticCategory::Test
        } else if lower.contains("no such package") || lower.contains("no such target") {
            DiagnosticCategory::Loading
        } else if lower.contains("visibility") && lower.contains("error") {
            DiagnosticCategory::Visibility
        } else if lower.contains("analysis") && (lower.contains("error") || lower.contains("fail"))
        {
            DiagnosticCategory::Analysis
        } else if lower.contains("missing strict dependencies") {
            DiagnosticCategory::Compilation
        } else {
            return None;
        };
    Some(Diagnostic {
        severity: if lower.contains("warning:") {
            Severity::Warning
        } else {
            Severity::Error
        },
        category,
        message: bounded_text(line, 64 * 1024),
        location: None,
        target: None,
        action: None,
        repetition_count: 1,
    })
}

fn split_u32_prefix(value: &str) -> Option<(u32, &str)> {
    let (number, remainder) = value.split_once(':')?;
    let number = number.trim();
    (!number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| number.parse::<u32>().ok().map(|number| (number, remainder)))
        .flatten()
}

pub(crate) fn compact_bazel_path(path: &str) -> String {
    let path = path.trim_matches('"').replace('\\', "/");
    let path = path
        .strip_prefix("<WORKSPACE>/")
        .or_else(|| path.strip_prefix("<workspace>/"))
        .unwrap_or(&path);
    for marker in [".runfiles/_main/", ".runfiles/__main__/"] {
        if let Some((_, relative)) = path.rsplit_once(marker) {
            return relative.to_owned();
        }
    }
    if let Some((_, after_execroot)) = path.rsplit_once("/execroot/")
        && let Some((_, relative)) = after_execroot.split_once('/')
    {
        return relative.to_owned();
    }
    path.strip_prefix("./").unwrap_or(path).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_only_bazel_owned_path_shapes() {
        assert_eq!(
            compact_bazel_path("/tmp/out/execroot/_main/pkg/main.go"),
            "pkg/main.go"
        );
        assert_eq!(
            compact_bazel_path("/tmp/test.runfiles/_main/pkg/test.py"),
            "pkg/test.py"
        );
        assert_eq!(compact_bazel_path("/opt/src/main.go"), "/opt/src/main.go");
    }

    #[test]
    fn parses_aspect_and_starlark_as_bazel_adapter_diagnostics() {
        let aspect = parse_aspect_lint(
            "🚨 src/app.ts:8:3 · eslint · no-console — unexpected console statement",
        )
        .unwrap();
        assert_eq!(aspect.category, DiagnosticCategory::Compilation);
        let starlark = parse_starlark_inline(
            "ERROR: /tmp/execroot/_main/pkg/rules.bzl:4:7: name 'missing' is not defined",
        )
        .unwrap();
        assert_eq!(starlark.category, DiagnosticCategory::Loading);
        assert_eq!(starlark.location.unwrap().path, "pkg/rules.bzl");
    }

    #[test]
    fn ignores_starlark_shaped_debug_telemetry() {
        let line = "DEBUG: /tmp/external/tool/extension.bzl:160:14: telemetry notice";
        assert!(parse_starlark_inline(line).is_none());
        let mut diagnostics = Vec::new();
        add_bazel_diagnostics(line, &mut diagnostics);
        assert!(diagnostics.is_empty());
    }
}
