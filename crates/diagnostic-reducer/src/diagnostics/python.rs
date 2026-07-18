use crate::{Diagnostic, DiagnosticClass, Location, Severity};

use super::common::normalize_path;

/// Stateful extractor for standard Python traceback and syntax-error output.
///
/// Python reports source locations on a `File "...", line N` frame before the
/// terminal exception. Keeping only the latest frame is bounded and matches
/// traceback semantics, where the innermost frame is printed last.
#[derive(Debug, Default)]
pub struct PythonDiagnosticParser {
    location: Option<Location>,
}

impl PythonDiagnosticParser {
    /// Observes one normalized output line and returns a diagnostic when the
    /// line terminates a Python exception block.
    pub fn observe_line(&mut self, line: &str) -> Option<Diagnostic> {
        if line.trim() == "Traceback (most recent call last):" {
            self.location = None;
            return None;
        }
        if let Some(location) = parse_location(line) {
            self.location = Some(location);
            return None;
        }
        let message = exception_message(line)?;
        let exception_type = message.split_once(':').map_or(message, |(name, _)| name);
        Some(Diagnostic {
            severity: if exception_type.ends_with("Warning") {
                Severity::Warning
            } else {
                Severity::Error
            },
            class: DiagnosticClass::Compiler,
            code: None,
            provenance: None,
            message: message.to_owned(),
            location: self.location.take(),
            quality: crate::EvidenceQuality::Structured,
            repetition_count: 1,
        })
    }
}

pub(super) fn parse_location(line: &str) -> Option<Location> {
    let marker = "File \"";
    let start = line.find(marker)? + marker.len();
    let remainder = &line[start..];
    let (path, remainder) = remainder.split_once("\", line ")?;
    let synthetic_path = path.starts_with('<') && !path.contains("/");
    if synthetic_path || path.ends_with("_stage2_bootstrap.py") {
        return None;
    }
    let digits = remainder
        .bytes()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if digits == 0 {
        return None;
    }
    let line_number = remainder[..digits].parse::<u32>().ok()?;
    Some(Location {
        path: compact_path(path),
        line: Some(line_number),
        column: None,
    })
}

pub(super) fn exception_message(line: &str) -> Option<&str> {
    let mut line = line.trim();
    if let Some(remainder) = line.strip_prefix('E')
        && remainder.chars().next().is_some_and(char::is_whitespace)
    {
        line = remainder.trim_start();
    }
    if line.contains("File \"") {
        return None;
    }
    let exception_type = line.split_once(':').map_or(line, |(name, _)| name);
    if exception_type.is_empty()
        || exception_type
            .bytes()
            .any(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.')))
    {
        return None;
    }
    let class_name = exception_type.rsplit('.').next()?;
    let recognized = class_name.ends_with("Error")
        || class_name.ends_with("Exception")
        || class_name.ends_with("Failure")
        || class_name.ends_with("Warning")
        || matches!(class_name, "Failed" | "KeyboardInterrupt" | "SystemExit");
    recognized.then_some(line)
}

fn compact_path(path: &str) -> String {
    normalize_path(path)
}
