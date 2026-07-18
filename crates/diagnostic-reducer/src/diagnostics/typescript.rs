use crate::{Diagnostic, DiagnosticClass, Location, Severity};

use super::javascript::compact_path;

pub(crate) fn parse_diagnostic(line: &str) -> Option<Diagnostic> {
    let line = line
        .trim()
        .strip_prefix("ERROR: ")
        .unwrap_or_else(|| line.trim());
    let (path, line_number, column, message) =
        parse_parenthesized_location(line).or_else(|| parse_pretty_location(line))?;
    let message = message.trim().trim_start_matches('-').trim();
    let (severity, message) = if let Some(message) = message.strip_prefix("error ") {
        (Severity::Error, message)
    } else {
        (Severity::Warning, message.strip_prefix("warning ")?)
    };
    let (code, message) = message.split_once(':')?;
    if !code.strip_prefix("TS").is_some_and(|digits| {
        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
    }) {
        return None;
    }
    let message = message.trim();
    if message.is_empty() {
        return None;
    }
    Some(Diagnostic {
        severity,
        class: DiagnosticClass::Compiler,
        code: None,
        provenance: None,
        message: format!("{code}: {message}"),
        location: Some(Location {
            path: compact_path(path),
            line: Some(line_number),
            column: Some(column),
        }),
        quality: crate::EvidenceQuality::Located,
        repetition_count: 1,
    })
}

fn parse_parenthesized_location(line: &str) -> Option<(&str, u32, u32, &str)> {
    let path_end = path_end(line, '(')?;
    let remainder = line[path_end..].strip_prefix('(')?;
    let (coordinates, message) = remainder.split_once("): ")?;
    let (line_number, column) = coordinates.split_once(',')?;
    let line_number = line_number.trim().parse::<u32>().ok()?;
    let column = column.trim().parse::<u32>().ok()?;
    Some((&line[..path_end], line_number, column, message))
}

fn parse_pretty_location(line: &str) -> Option<(&str, u32, u32, &str)> {
    let path_end = path_end(line, ':')?;
    let (line_number, remainder) = line[path_end + 1..].split_once(':')?;
    let line_number = line_number.parse::<u32>().ok()?;
    let (column, message) = remainder
        .split_once(" - ")
        .or_else(|| remainder.split_once(':'))?;
    let column = column.parse::<u32>().ok()?;
    Some((&line[..path_end], line_number, column, message))
}

fn path_end(line: &str, delimiter: char) -> Option<usize> {
    const EXTENSIONS: [&str; 8] = [".tsx", ".mts", ".cts", ".ts", ".jsx", ".mjs", ".cjs", ".js"];
    EXTENSIONS
        .iter()
        .filter_map(|extension| {
            let marker = format!("{extension}{delimiter}");
            line.rfind(&marker).map(|index| index + extension.len())
        })
        .max()
}
