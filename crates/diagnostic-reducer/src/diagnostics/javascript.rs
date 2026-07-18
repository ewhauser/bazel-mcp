use std::collections::BTreeSet;

use crate::{Diagnostic, DiagnosticClass, Location, Severity};

use super::common::{bounded_text, normalize_path};

#[derive(Debug, Default)]
pub(crate) struct SwcParseOutput {
    pub(crate) diagnostics: Vec<Diagnostic>,
    pub(crate) consumed_lines: BTreeSet<String>,
}

pub(super) fn reduce_swc(input: &str, output: &mut SwcParseOutput) {
    *output = parse_swc_diagnostics(input);
}

pub(crate) fn parse_swc_diagnostics(input: &str) -> SwcParseOutput {
    const MAX_HEADER_DISTANCE: usize = 3;
    const MAX_FRAME_LINES: usize = 8;

    let lines = input.lines().collect::<Vec<_>>();
    let mut output = SwcParseOutput::default();
    for (location_index, line) in lines.iter().enumerate() {
        let Some(location) = parse_swc_source_frame_location(line) else {
            continue;
        };
        let Some((header_index, severity, message)) = lines[..location_index]
            .iter()
            .enumerate()
            .rev()
            .take(MAX_HEADER_DISTANCE)
            .find_map(|(index, line)| {
                parse_swc_message_header(line).map(|(severity, message)| (index, severity, message))
            })
        else {
            continue;
        };

        let mut frame_end = location_index;
        let mut source_line = None;
        let mut first_source_line = None;
        let mut caret_line = None;
        for (index, frame_line) in lines
            .iter()
            .enumerate()
            .skip(location_index + 1)
            .take(MAX_FRAME_LINES)
        {
            if parse_swc_message_header(frame_line).is_some()
                || parse_swc_source_frame_location(frame_line).is_some()
            {
                break;
            }
            frame_end = index;
            output.consumed_lines.insert(frame_line.trim().to_owned());
            if let Some(line_number) = swc_source_line_number(frame_line) {
                let bounded = bounded_text(frame_line.trim(), 256);
                first_source_line.get_or_insert_with(|| bounded.clone());
                if location.line == Some(line_number) {
                    source_line = Some(bounded);
                }
            } else if caret_line.is_none() && is_swc_caret_line(frame_line) {
                caret_line = Some(bounded_text(frame_line.trim(), 256));
            }
            if is_swc_frame_terminator(frame_line) {
                break;
            }
        }

        output
            .consumed_lines
            .insert(lines[header_index].trim().to_owned());
        output
            .consumed_lines
            .insert(lines[location_index].trim().to_owned());
        let (context, context_lines) = swc_failure_context(&lines, header_index, frame_end);
        for context_line in context_lines {
            output
                .consumed_lines
                .insert(lines[context_line].trim().to_owned());
        }

        let mut message = bounded_text(message, 1_024);
        if let Some(context) = context {
            message.push_str("\nSWC context: ");
            message.push_str(&bounded_text(&context, 512));
        }
        if let Some(source_line) = source_line.or(first_source_line) {
            message.push('\n');
            message.push_str(&source_line);
        }
        if let Some(caret_line) = caret_line {
            message.push('\n');
            message.push_str(&caret_line);
        }
        output.diagnostics.push(Diagnostic {
            severity,
            class: DiagnosticClass::Compiler,
            code: Some("swc.parser".to_owned()),
            provenance: None,
            message,
            location: Some(location),
            quality: crate::EvidenceQuality::Located,
            repetition_count: 1,
        });
    }
    output
}

fn parse_swc_message_header(line: &str) -> Option<(Severity, &str)> {
    let line = line.trim();
    let (severity, message) = if let Some(message) = line.strip_prefix("error:") {
        (Severity::Error, message)
    } else if let Some(message) = line.strip_prefix("warning:") {
        (Severity::Warning, message)
    } else if let Some(message) = strip_swc_icon(line, 'x').or_else(|| strip_swc_icon(line, '×')) {
        (Severity::Error, message)
    } else {
        (Severity::Warning, strip_swc_icon(line, '!')?)
    };
    let message = message.trim();
    (!message.is_empty()).then_some((severity, message))
}

fn strip_swc_icon(line: &str, icon: char) -> Option<&str> {
    let remainder = line.strip_prefix(icon)?;
    remainder
        .chars()
        .next()
        .is_some_and(char::is_whitespace)
        .then(|| remainder.trim_start())
}

fn parse_swc_source_frame_location(line: &str) -> Option<Location> {
    let line = line.trim();
    let opening = line.find('[')?;
    let marker = line[..opening].trim_end();
    if !marker.ends_with(",-") && !marker.ends_with("╭─") {
        return None;
    }
    let closing = line[opening + 1..].find(']')? + opening + 1;
    let coordinates = &line[opening + 1..closing];
    let (path_and_line, column) = coordinates.rsplit_once(':')?;
    let (path, line_number) = path_and_line.rsplit_once(':')?;
    if path.trim().is_empty() {
        return None;
    }
    Some(Location {
        path: compact_path(path.trim()),
        line: Some(line_number.parse::<u32>().ok()?),
        column: Some(column.parse::<u32>().ok()?),
    })
}

fn swc_source_line_number(line: &str) -> Option<u32> {
    let line = line.trim();
    let (line_number, _) = line.split_once('|').or_else(|| line.split_once('│'))?;
    let line_number = line_number.trim();
    (!line_number.is_empty() && line_number.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| line_number.parse::<u32>().ok())
        .flatten()
}

fn is_swc_caret_line(line: &str) -> bool {
    let line = line.trim();
    line.contains('^')
        && (line.starts_with(':')
            || line
                .split_once('|')
                .is_some_and(|(prefix, _)| prefix.trim().is_empty()))
}

fn is_swc_frame_terminator(line: &str) -> bool {
    let line = line.trim();
    (line.starts_with('`') && line.contains("---")) || (line.starts_with('╰') && line.contains('─'))
}

fn swc_failure_context(
    lines: &[&str],
    header_index: usize,
    frame_end: usize,
) -> (Option<String>, Vec<usize>) {
    const MAX_CONTEXT_DISTANCE: usize = 12;
    const MAX_CONTEXT_ITEMS: usize = 2;

    let mut contexts = Vec::new();
    let mut consumed = Vec::new();
    let preceding_start = header_index.saturating_sub(MAX_CONTEXT_DISTANCE);
    for (index, line) in lines
        .iter()
        .enumerate()
        .take(header_index)
        .skip(preceding_start)
    {
        if let Some(context) = parse_swc_context_line(line)
            && !contexts.contains(&context)
        {
            contexts.push(context);
            consumed.push(index);
        }
        if contexts.len() >= MAX_CONTEXT_ITEMS {
            break;
        }
    }
    for (index, line) in lines
        .iter()
        .enumerate()
        .skip(frame_end + 1)
        .take(MAX_CONTEXT_DISTANCE)
    {
        if parse_swc_message_header(line).is_some()
            || parse_swc_source_frame_location(line).is_some()
        {
            break;
        }
        if let Some(context) = parse_swc_context_line(line)
            && !contexts.contains(&context)
        {
            contexts.push(context);
            consumed.push(index);
        }
        if contexts.len() >= MAX_CONTEXT_ITEMS {
            break;
        }
    }
    contexts.truncate(MAX_CONTEXT_ITEMS);
    (
        (!contexts.is_empty()).then(|| contexts.join("; ")),
        consumed,
    )
}

fn parse_swc_context_line(line: &str) -> Option<String> {
    if parse_swc_message_header(line).is_some() {
        return None;
    }
    let mut line = line.trim();
    if let Some(remainder) = line.strip_prefix("Caused by:") {
        line = remainder.trim();
    }
    if let Some((number, remainder)) = line.split_once(": ")
        && !number.is_empty()
        && number.bytes().all(|byte| byte.is_ascii_digit())
    {
        line = remainder.trim();
    }
    let lower = line.to_ascii_lowercase();
    let is_context = lower.starts_with("failed to process")
        || lower.starts_with("failed to parse")
        || lower.starts_with("failed to transform")
        || lower.starts_with("failed to invoke plugin")
        || lower.starts_with("failed to handle")
        || (lower.contains("plugin") && lower.contains("failed"));
    (is_context && !line.is_empty()).then(|| line.to_owned())
}

pub(super) fn is_swc_action_wrapper(line: &str) -> bool {
    let line = line.trim();
    let lower = line.to_ascii_lowercase();
    line.starts_with("ERROR:")
        && (lower.contains("build did not complete successfully")
            || lower.contains("failed: error executing")
            || (lower.contains("swc")
                && (lower.contains("failed:") || lower.contains("action failed"))))
}

pub(super) fn reduce_tests(
    input: &str,
    diagnostics: &mut Vec<Diagnostic>,
    messages: &mut BTreeSet<String>,
) {
    let mut parser = JavaScriptTestDiagnosticParser::default();
    for line in input.lines() {
        if let Some(diagnostic) = parser.observe_line(line) {
            messages.insert(diagnostic.message.clone());
            diagnostics.push(diagnostic);
        }
    }
    if let Some(diagnostic) = parser.finish() {
        messages.insert(diagnostic.message.clone());
        diagnostics.push(diagnostic);
    }
}

/// Stateful extractor for Node.js exceptions and their application frames.
#[derive(Debug, Default)]
pub struct JavaScriptTestDiagnosticParser {
    leading_location: Option<Location>,
    pending: Option<Diagnostic>,
    frames_seen: usize,
}

impl JavaScriptTestDiagnosticParser {
    const MAX_STACK_FRAMES: usize = 64;

    /// Observes one normalized test-log line and emits an exception after a
    /// JavaScript source header or application stack frame confirms it.
    pub fn observe_line(&mut self, line: &str) -> Option<Diagnostic> {
        if !line.trim_start().starts_with("at ")
            && let Some(location) = parse_location(line.trim())
        {
            let previous = self.take_confirmed();
            self.leading_location = Some(location);
            return previous;
        }
        if let Some(message) = exception_message(line) {
            let leading_location = self.leading_location.take();
            let previous = self.take_confirmed();
            self.pending = Some(Diagnostic {
                severity: Severity::Error,
                class: DiagnosticClass::Test,
                code: None,
                provenance: None,
                message: message.to_owned(),
                location: leading_location,
                quality: crate::EvidenceQuality::Structured,
                repetition_count: 1,
            });
            self.frames_seen = 0;
            return previous;
        }
        self.pending.as_ref()?;
        if let Some(location) = parse_stack_frame(line) {
            self.frames_seen = self.frames_seen.saturating_add(1);
            if let Some(location) = location {
                let mut diagnostic = self.pending.take()?;
                diagnostic.location = Some(location);
                self.frames_seen = 0;
                return Some(diagnostic);
            }
            if self.frames_seen >= Self::MAX_STACK_FRAMES {
                return self.take_confirmed();
            }
            return None;
        }
        if line.trim().is_empty() {
            return None;
        }
        self.take_confirmed()
    }

    /// Emits a confirmed exception that reached end-of-file.
    pub fn finish(&mut self) -> Option<Diagnostic> {
        self.take_confirmed()
    }

    fn take_confirmed(&mut self) -> Option<Diagnostic> {
        let confirmed = self
            .pending
            .as_ref()
            .is_some_and(|diagnostic| diagnostic.location.is_some())
            || self.frames_seen > 0;
        self.frames_seen = 0;
        self.leading_location = None;
        if confirmed {
            self.pending.take()
        } else {
            self.pending = None;
            None
        }
    }
}

pub(super) fn exception_message(line: &str) -> Option<&str> {
    let line = line.trim();
    let exception_type = line.split_once(':').map_or(line, |(name, _)| name);
    let class_name = exception_type.split_whitespace().next()?;
    (!class_name.is_empty()
        && class_name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'$'))
        && (class_name.ends_with("Error") || class_name.ends_with("Exception")))
    .then_some(line)
}

fn parse_stack_frame(line: &str) -> Option<Option<Location>> {
    let frame = line.trim().strip_prefix("at ")?;
    let source = if let Some((_, source)) = frame.rsplit_once('(') {
        source.strip_suffix(')')?
    } else {
        frame.split_whitespace().last()?
    };
    if source.starts_with("node:") || matches!(source, "native" | "<anonymous>") {
        return Some(None);
    }
    let location = parse_location(source)?;
    let framework = location.path.contains("/node_modules/")
        || location.path.starts_with("node_modules/")
        || location.path.contains("/external/");
    Some((!framework).then_some(location))
}

fn parse_location(value: &str) -> Option<Location> {
    let value = value.trim().trim_matches('"');
    let path_end = path_end(value)?;
    let coordinates = value[path_end..].strip_prefix(':')?;
    let (line_number, column) = if let Some((line_number, column)) = coordinates.split_once(':') {
        (
            line_number.parse::<u32>().ok()?,
            Some(column.parse::<u32>().ok()?),
        )
    } else {
        (coordinates.parse::<u32>().ok()?, None)
    };
    Some(Location {
        path: compact_path(&value[..path_end]),
        line: Some(line_number),
        column,
    })
}

fn path_end(value: &str) -> Option<usize> {
    const EXTENSIONS: [&str; 8] = [".tsx", ".mts", ".cts", ".ts", ".jsx", ".mjs", ".cjs", ".js"];
    EXTENSIONS
        .iter()
        .filter_map(|extension| {
            let marker = format!("{extension}:");
            value.rfind(&marker).map(|index| index + extension.len())
        })
        .max()
}

pub(super) fn compact_path(path: &str) -> String {
    let path = path
        .trim_matches('"')
        .strip_prefix("file://")
        .unwrap_or(path)
        .to_owned();
    normalize_path(&path)
}
