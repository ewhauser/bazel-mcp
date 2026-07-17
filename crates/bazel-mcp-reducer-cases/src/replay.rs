use std::{collections::BTreeMap, fs, path::Path};

use anyhow::{Context, Result, ensure};
use bazel_mcp_bep::{DEFAULT_MAX_FRAME_BYTES, decode_stream_partial};
use bazel_mcp_reducer::{
    BepAccumulator, Budget, JavaScriptTestDiagnosticParser, JavaTestDiagnosticParser,
    PythonDiagnosticParser, ReductionInput, TestFailureAccumulator, finalize_diagnostics,
    normalize_terminal_text, parse_go_diagnostic, reduce_artifacts, reduce_invocation,
};
use bazel_mcp_types::{
    Artifact, Diagnostic, DiagnosticCategory, DiagnosticLocation, InspectHint, InvocationSummary,
    Severity,
};
use serde::{Deserialize, Serialize};

use crate::{
    EvidenceSpec, LoadedCase, observe_replay, verify_expectations, verify_sanitized_evidence,
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReplayOutput {
    pub event_count: usize,
    pub terminal_error: Option<String>,
    pub summary: InvocationSummary,
    pub artifacts: Vec<Artifact>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordedProvenance {
    schema_version: u32,
    case: String,
    workspace: String,
    command: String,
    args: Vec<String>,
    bazel_version: Option<String>,
    platform: String,
    architecture: String,
    tool: String,
    tool_version: String,
    rules: BTreeMap<String, String>,
    origin_repository: Option<String>,
    origin_commit: Option<String>,
}

pub fn replay_case(case: &LoadedCase) -> Result<ReplayOutput> {
    let evidence = case
        .manifest
        .evidence
        .as_ref()
        .context("case has no recorded evidence")?;
    replay_with_evidence(case, evidence)
}

pub fn verify_recorded_case(case: &LoadedCase) -> Result<ReplayOutput> {
    let evidence = case
        .manifest
        .evidence
        .as_ref()
        .context("case has no recorded evidence")?;
    verify_case_evidence(case, evidence)
}

pub fn verify_case_evidence(case: &LoadedCase, evidence: &EvidenceSpec) -> Result<ReplayOutput> {
    let output = verify_case_contract(case, evidence)?;
    let actual = serde_json::to_string_pretty(&output)? + "\n";
    let expected_path = case.evidence_path(&evidence.expected);
    let expected = fs::read_to_string(&expected_path)
        .with_context(|| format!("read expected golden {}", expected_path.display()))?;
    ensure!(
        actual == expected,
        "golden changed for {}: run record --replay-only, review actual.expected.json, and explicitly accept",
        case.manifest.id
    );
    Ok(output)
}

pub fn verify_case_contract(case: &LoadedCase, evidence: &EvidenceSpec) -> Result<ReplayOutput> {
    for (name, relative) in evidence.paths() {
        let path = case.evidence_path(relative);
        let bytes = fs::read(&path)
            .with_context(|| format!("read {name} for sanitization check {}", path.display()))?;
        verify_sanitized_evidence(&bytes)
            .with_context(|| format!("{name} is not safely sanitized in {}", path.display()))?;
    }
    let output = replay_with_evidence(case, evidence)?;
    let exit_path = case.evidence_path(&evidence.exit);
    let exit_code = fs::read_to_string(&exit_path)
        .with_context(|| format!("read exit evidence {}", exit_path.display()))?
        .trim()
        .parse::<i32>()
        .with_context(|| format!("parse exit evidence {}", exit_path.display()))?;
    let mut raw = String::new();
    for path in [&evidence.stdout, &evidence.stderr]
        .into_iter()
        .chain(evidence.test_logs.iter())
    {
        let path = case.evidence_path(path);
        raw.push_str(&String::from_utf8_lossy(
            &fs::read(&path).with_context(|| format!("read evidence {}", path.display()))?,
        ));
    }
    let observation = observe_replay(&output, exit_code, raw);
    verify_expectations(&case.manifest.id, &case.manifest.expect, &observation)?;
    verify_provenance(case, evidence)?;
    Ok(output)
}

fn verify_provenance(case: &LoadedCase, evidence: &EvidenceSpec) -> Result<()> {
    let path = case.evidence_path(&evidence.provenance);
    let bytes =
        fs::read(&path).with_context(|| format!("read provenance file {}", path.display()))?;
    let provenance: RecordedProvenance = serde_json::from_slice(&bytes)
        .with_context(|| format!("decode provenance file {}", path.display()))?;
    ensure!(
        provenance.schema_version == 1,
        "unsupported provenance schema"
    );
    ensure!(
        provenance.case == case.manifest.id,
        "provenance case differs from manifest"
    );
    ensure!(
        Path::new(&provenance.workspace) == case.manifest.workspace,
        "provenance workspace differs from manifest"
    );
    ensure!(
        provenance.command == case.manifest.command,
        "provenance command differs from manifest"
    );
    ensure!(
        provenance.args == case.manifest.args,
        "provenance args differ from manifest"
    );
    ensure!(
        provenance.bazel_version.is_some(),
        "provenance must record the effective Bazel version"
    );
    ensure!(
        !provenance.platform.trim().is_empty(),
        "provenance platform is empty"
    );
    ensure!(
        !provenance.architecture.trim().is_empty(),
        "provenance architecture is empty"
    );
    ensure!(
        provenance.tool == case.manifest.provenance.tool,
        "provenance tool differs from manifest"
    );
    ensure!(
        provenance.tool_version == case.manifest.provenance.tool_version,
        "provenance tool version differs from manifest"
    );
    ensure!(
        provenance.rules == case.manifest.provenance.rules,
        "provenance rules differ from manifest"
    );
    ensure!(
        provenance.origin_repository == case.manifest.provenance.origin_repository,
        "provenance origin repository differs from manifest"
    );
    ensure!(
        provenance.origin_commit == case.manifest.provenance.origin_commit,
        "provenance origin commit differs from manifest"
    );
    Ok(())
}

pub fn replay_with_evidence(case: &LoadedCase, evidence: &EvidenceSpec) -> Result<ReplayOutput> {
    let bep_path = case.evidence_path(&evidence.bep);
    let partial = decode_stream_partial(
        fs::File::open(&bep_path)
            .with_context(|| format!("open BEP evidence {}", bep_path.display()))?,
        DEFAULT_MAX_FRAME_BYTES,
    );
    let stdout_path = case.evidence_path(&evidence.stdout);
    let stderr_path = case.evidence_path(&evidence.stderr);
    let exit_path = case.evidence_path(&evidence.exit);
    let stdout = fs::read(&stdout_path)
        .with_context(|| format!("read stdout evidence {}", stdout_path.display()))?;
    let stderr = fs::read(&stderr_path)
        .with_context(|| format!("read stderr evidence {}", stderr_path.display()))?;
    let exit_code = fs::read_to_string(&exit_path)
        .with_context(|| format!("read exit evidence {}", exit_path.display()))?
        .trim()
        .parse::<i32>()
        .with_context(|| format!("parse exit evidence {}", exit_path.display()))?;
    let budget = Budget {
        max_items: case.manifest.replay.max_items,
        max_bytes: case.manifest.replay.max_bytes,
    };
    let mut summary = reduce_invocation(ReductionInput {
        events: &partial.events,
        stdout: &stdout,
        stderr: &stderr,
        exit_code: Some(exit_code),
        elapsed_ms: 0,
        budget,
    });
    let artifacts = reduce_artifacts(&partial.events);

    let mut accumulator = BepAccumulator::default();
    for event in partial.events.iter().cloned() {
        accumulator.observe(event);
    }
    let streaming = accumulator.finish(&stdout, &stderr, Some(exit_code), 0, budget);
    ensure!(
        streaming.summary == summary,
        "case {} batch and streaming summaries differ",
        case.manifest.id
    );
    ensure!(
        streaming.artifacts == artifacts,
        "case {} batch and streaming artifacts differ",
        case.manifest.id
    );

    if !evidence.test_logs.is_empty() {
        let target = case
            .manifest
            .test_target
            .as_deref()
            .context("recorded test logs require test_target")?;
        for log in &evidence.test_logs {
            enrich_from_test_log(&case.evidence_path(log), target, &mut summary)?;
        }
        finalize_diagnostics(&mut summary, budget);
        if !summary.success && summary.inspect_hint.is_none() {
            summary.inspect_hint = Some(InspectHint::TestLog);
        }
    }

    summary.elapsed_ms = 0;
    for test in &mut summary.tests {
        test.duration_ms = None;
        for case in &mut test.cases {
            case.duration_ms = None;
        }
    }

    Ok(ReplayOutput {
        event_count: partial.events.len(),
        terminal_error: partial.terminal_error.as_ref().map(ToString::to_string),
        summary,
        artifacts,
    })
}

fn enrich_from_test_log(path: &Path, target: &str, summary: &mut InvocationSummary) -> Result<()> {
    let contents = fs::read(path).with_context(|| format!("read test log {}", path.display()))?;
    let normalized = normalize_terminal_text(&contents);
    let mut accumulator = TestFailureAccumulator::default();
    let mut javascript = JavaScriptTestDiagnosticParser::default();
    let mut java = JavaTestDiagnosticParser::default();
    let mut python = PythonDiagnosticParser::default();
    for line in normalized.lines() {
        accumulator.observe_line(line);
        if let Some(diagnostic) = parse_go_diagnostic(line) {
            push_test_diagnostic(summary, target, diagnostic);
        }
        if let Some(diagnostic) = javascript.observe_line(line) {
            push_test_diagnostic(summary, target, diagnostic);
        }
        if let Some(diagnostic) = java.observe_line(line) {
            push_test_diagnostic(summary, target, diagnostic);
        }
        if let Some(diagnostic) = python.observe_line(line) {
            push_test_diagnostic(summary, target, diagnostic);
        }
    }
    if let Some(diagnostic) = javascript.finish() {
        push_test_diagnostic(summary, target, diagnostic);
    }
    if let Some(diagnostic) = java.finish() {
        push_test_diagnostic(summary, target, diagnostic);
    }
    for failure in accumulator.finish() {
        summary.diagnostics.push(Diagnostic {
            severity: Severity::Error,
            category: DiagnosticCategory::Test,
            message: failure.message,
            location: failure.location.map(|location| DiagnosticLocation {
                path: location.path,
                line: location.line,
                column: location.column,
            }),
            target: Some(target.to_owned()),
            action: None,
            repetition_count: 1,
        });
    }
    Ok(())
}

fn push_test_diagnostic(summary: &mut InvocationSummary, target: &str, mut diagnostic: Diagnostic) {
    diagnostic.category = DiagnosticCategory::Test;
    diagnostic.target = Some(target.to_owned());
    summary.diagnostics.retain(|existing| {
        !((existing.category == DiagnosticCategory::Compilation
            || (existing.category == diagnostic.category && existing.target.is_none()))
            && existing.message == diagnostic.message
            && (existing.location == diagnostic.location || existing.location.is_none()))
    });
    summary.diagnostics.push(diagnostic);
}
