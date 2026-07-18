use diagnostic_reducer::{Diagnostic, Reduction, Severity, TestFailure};
use serde_json::{Value, json};

/// Stable presentation formats supported by the standalone adapter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputFormat {
    Human,
    Json,
    JsonLines,
    Sarif,
    GitHub,
}

#[must_use]
pub fn render(reduction: &Reduction, format: OutputFormat) -> String {
    match format {
        OutputFormat::Human => render_human(reduction),
        OutputFormat::Json => serde_json::to_string_pretty(reduction)
            .expect("the reduction data model is always serializable"),
        OutputFormat::JsonLines => render_json_lines(reduction),
        OutputFormat::Sarif => serde_json::to_string_pretty(&sarif(reduction))
            .expect("the generated SARIF value is always serializable"),
        OutputFormat::GitHub => render_github(reduction),
    }
}

#[must_use]
pub fn has_severity(reduction: &Reduction, minimum: Severity) -> bool {
    reduction
        .diagnostics
        .iter()
        .any(|diagnostic| severity_rank(diagnostic.severity) <= severity_rank(minimum))
        || (minimum == Severity::Error && !reduction.test_failures.is_empty())
}

fn severity_rank(severity: Severity) -> u8 {
    match severity {
        Severity::Error => 0,
        Severity::Warning => 1,
        Severity::Note => 2,
    }
}

fn render_human(reduction: &Reduction) -> String {
    let mut output = String::new();
    for diagnostic in &reduction.diagnostics {
        let severity = match diagnostic.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Note => "note",
        };
        if let Some(location) = &diagnostic.location {
            output.push_str(&location.path);
            if let Some(line) = location.line {
                output.push(':');
                output.push_str(&line.to_string());
            }
            if let Some(column) = location.column {
                output.push(':');
                output.push_str(&column.to_string());
            }
            output.push_str(": ");
        }
        output.push_str(severity);
        output.push_str(": ");
        output.push_str(&diagnostic.message);
        if diagnostic.repetition_count > 1 {
            output.push_str(" [repeated ");
            output.push_str(&diagnostic.repetition_count.to_string());
            output.push(']');
        }
        output.push('\n');
    }
    for failure in &reduction.test_failures {
        output.push_str("test failed: ");
        output.push_str(&failure.name);
        output.push_str(": ");
        output.push_str(&failure.message);
        output.push('\n');
    }
    if reduction.truncated {
        output.push_str("note: reduction was truncated\n");
    }
    output
}

fn render_json_lines(reduction: &Reduction) -> String {
    let mut lines = Vec::new();
    for diagnostic in &reduction.diagnostics {
        lines.push(
            serde_json::to_string(&json!({"type": "diagnostic", "value": diagnostic}))
                .expect("a diagnostic is always serializable"),
        );
    }
    for failure in &reduction.test_failures {
        lines.push(
            serde_json::to_string(&json!({"type": "test_failure", "value": failure}))
                .expect("a test failure is always serializable"),
        );
    }
    lines.push(
        serde_json::to_string(&json!({
            "type": "summary",
            "truncated": reduction.truncated,
            "omitted_diagnostics": reduction.omitted_diagnostics,
            "used_bytes": reduction.used_bytes,
            "stats": reduction.stats,
        }))
        .expect("summary accounting is always serializable"),
    );
    let mut output = lines.join("\n");
    output.push('\n');
    output
}

fn render_github(reduction: &Reduction) -> String {
    let mut output = String::new();
    for diagnostic in &reduction.diagnostics {
        let command = match diagnostic.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Note => "notice",
        };
        output.push_str("::");
        output.push_str(command);
        if let Some(location) = &diagnostic.location {
            output.push_str(" file=");
            output.push_str(&escape_property(&location.path));
            if let Some(line) = location.line {
                output.push_str(",line=");
                output.push_str(&line.to_string());
            }
            if let Some(column) = location.column {
                output.push_str(",col=");
                output.push_str(&column.to_string());
            }
        }
        output.push_str("::");
        output.push_str(&escape_message(&diagnostic.message));
        output.push('\n');
    }
    for failure in &reduction.test_failures {
        output.push_str("::error title=");
        output.push_str(&escape_property(&failure.name));
        output.push_str("::");
        output.push_str(&escape_message(&failure.message));
        output.push('\n');
    }
    output
}

fn sarif(reduction: &Reduction) -> Value {
    let mut rules = Vec::<Value>::new();
    let mut rule_ids = Vec::<String>::new();
    let results = reduction
        .diagnostics
        .iter()
        .map(|diagnostic| {
            let rule_id = diagnostic
                .code
                .clone()
                .unwrap_or_else(|| "diagnostic-reducer.finding".to_owned());
            if !rule_ids.contains(&rule_id) {
                rule_ids.push(rule_id.clone());
                rules.push(json!({
                    "id": rule_id,
                    "shortDescription": {"text": "Reduced log diagnostic"}
                }));
            }
            sarif_result(diagnostic, &rule_id)
        })
        .chain(reduction.test_failures.iter().map(sarif_test_failure))
        .collect::<Vec<_>>();
    json!({
        "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
        "version": "2.1.0",
        "runs": [{
            "tool": {"driver": {"name": "diagnostic-reducer", "rules": rules}},
            "results": results,
            "properties": {
                "truncated": reduction.truncated,
                "omittedDiagnostics": reduction.omitted_diagnostics
            }
        }]
    })
}

fn sarif_result(diagnostic: &Diagnostic, rule_id: &str) -> Value {
    let level = match diagnostic.severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Note => "note",
    };
    let locations = diagnostic
        .location
        .as_ref()
        .map_or_else(Vec::new, |location| {
            vec![json!({
                "physicalLocation": {
                    "artifactLocation": {"uri": location.path},
                    "region": {
                        "startLine": location.line.unwrap_or(1),
                        "startColumn": location.column.unwrap_or(1)
                    }
                }
            })]
        });
    json!({
        "ruleId": rule_id,
        "level": level,
        "message": {"text": diagnostic.message},
        "locations": locations,
        "properties": {
            "class": diagnostic.class,
            "quality": diagnostic.quality,
            "repetitionCount": diagnostic.repetition_count,
            "provenance": diagnostic.provenance
        }
    })
}

fn sarif_test_failure(failure: &TestFailure) -> Value {
    let locations = failure.location.as_ref().map_or_else(Vec::new, |location| {
        vec![json!({
            "physicalLocation": {
                "artifactLocation": {"uri": location.path},
                "region": {"startLine": location.line.unwrap_or(1)}
            }
        })]
    });
    json!({
        "ruleId": "test.failure",
        "level": "error",
        "message": {"text": failure.message},
        "locations": locations,
        "properties": {
            "testName": failure.name,
            "framework": failure.framework,
            "provenance": failure.provenance
        }
    })
}

fn escape_message(value: &str) -> String {
    value
        .replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

fn escape_property(value: &str) -> String {
    escape_message(value)
        .replace(':', "%3A")
        .replace(',', "%2C")
}

#[cfg(test)]
mod tests {
    use diagnostic_reducer::{DiagnosticClass, EvidenceQuality, Location, ReductionStats};

    use super::*;

    fn reduction() -> Reduction {
        Reduction {
            diagnostics: vec![Diagnostic {
                severity: Severity::Error,
                class: DiagnosticClass::Compiler,
                code: Some("rust.E0308".to_owned()),
                message: "mismatched types".to_owned(),
                location: Some(Location {
                    path: "src/lib.rs".to_owned(),
                    line: Some(7),
                    column: Some(5),
                }),
                provenance: None,
                quality: EvidenceQuality::Located,
                repetition_count: 1,
            }],
            test_failures: Vec::new(),
            truncated: false,
            omitted_diagnostics: 0,
            used_bytes: 10,
            stats: ReductionStats::default(),
        }
    }

    #[test]
    fn renders_human_github_and_sarif_locations() {
        let reduction = reduction();
        assert!(render(&reduction, OutputFormat::Human).contains("src/lib.rs:7:5"));
        assert!(render(&reduction, OutputFormat::GitHub).contains("file=src/lib.rs,line=7,col=5"));
        let sarif = render(&reduction, OutputFormat::Sarif);
        assert!(sarif.contains("\"version\": \"2.1.0\""));
        assert!(sarif.contains("src/lib.rs"));
    }

    #[test]
    fn json_lines_ends_with_summary() {
        let rendered = render(&reduction(), OutputFormat::JsonLines);
        assert_eq!(rendered.lines().count(), 2);
        assert!(rendered.lines().last().unwrap().contains("summary"));
    }
}
