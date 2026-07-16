use std::{collections::BTreeSet, hint::black_box, sync::Arc};

use bazel_mcp_reducer::{
    CustomReducer, REDUCER_API_VERSION, ReducerContext, ReducerError, ReducerMode, ReducerPatch,
    ReducerPipeline, ReducerSelector, StarlarkLimits, StarlarkReducerConfig,
    load_starlark_reducers,
};
use bazel_mcp_types::{
    Diagnostic, DiagnosticCategory, DiagnosticLocation, InvocationSummary, Severity,
};
use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use regex::Regex;

const PATTERN: &str =
    r"(?m)^(?P<path>[^:\n]+):(?P<line>[0-9]+):(?P<column>[0-9]+): error: (?P<message>.+)$";

struct NativeRegexReducer {
    selector: ReducerSelector,
    pattern: Regex,
}

impl NativeRegexReducer {
    fn new() -> Self {
        Self {
            selector: ReducerSelector {
                commands: BTreeSet::from(["build".to_owned()]),
                ..ReducerSelector::default()
            },
            pattern: Regex::new(PATTERN).unwrap(),
        }
    }
}

impl CustomReducer for NativeRegexReducer {
    fn name(&self) -> &str {
        "native-regex"
    }

    fn priority(&self) -> i32 {
        0
    }

    fn mode(&self) -> ReducerMode {
        ReducerMode::Augment
    }

    fn selector(&self) -> &ReducerSelector {
        &self.selector
    }

    fn reduce(&self, context: &ReducerContext) -> Result<ReducerPatch, ReducerError> {
        let diagnostics = self
            .pattern
            .captures_iter(&context.stderr)
            .map(|captures| Diagnostic {
                severity: Severity::Error,
                category: DiagnosticCategory::Compilation,
                message: captures["message"].to_owned(),
                location: Some(DiagnosticLocation {
                    path: captures["path"].to_owned(),
                    line: captures["line"].parse().ok(),
                    column: captures["column"].parse().ok(),
                }),
                target: None,
                action: None,
                repetition_count: 1,
            })
            .collect();
        Ok(ReducerPatch {
            headline: Some("Compiler failed".to_owned()),
            diagnostics,
            suppress_builtin_diagnostics: false,
        })
    }
}

fn context(lines: usize) -> ReducerContext {
    let stderr = (0..lines)
        .map(|index| {
            format!(
                "src/file{index}.cc:{}:7: error: missing value {index}",
                index + 1
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    ReducerContext {
        api_version: REDUCER_API_VERSION,
        command: "build".to_owned(),
        arguments: vec!["//app:target".to_owned()],
        exit_code: Some(1),
        elapsed_ms: 100,
        stdout: String::new(),
        stderr,
        events: Vec::new(),
        input_truncated: false,
        baseline: InvocationSummary::default(),
    }
}

fn starlark_pipeline(root: &tempfile::TempDir) -> ReducerPipeline {
    let path = root.path().join("compiler.star");
    std::fs::write(
        &path,
        format!(
            r#"
API_VERSION = 1
NAME = "starlark-regex"
COMMANDS = ["build"]

def reduce(ctx):
    diagnostics = regex_diagnostics(
        ctx["stderr"],
        r"{PATTERN}",
        category = "compilation",
        max_matches = 1000,
    )
    return patch(diagnostics, headline = "Compiler failed")
"#
        ),
    )
    .unwrap();
    load_starlark_reducers(&StarlarkReducerConfig {
        files: vec![path],
        limits: StarlarkLimits {
            timeout: std::time::Duration::from_secs(1),
            ..StarlarkLimits::default()
        },
    })
    .unwrap()
}

fn custom_reducers(c: &mut Criterion) {
    let native = ReducerPipeline::new(vec![Arc::new(NativeRegexReducer::new())]).unwrap();
    let root = tempfile::tempdir().unwrap();
    let starlark = starlark_pipeline(&root);
    let mut group = c.benchmark_group("custom_reducers");
    for lines in [1_usize, 100, 1_000] {
        let context = context(lines);
        group.throughput(Throughput::Bytes(context.stderr.len() as u64));
        group.bench_function(format!("native/{lines}"), |b| {
            b.iter(|| {
                let mut summary = InvocationSummary::default();
                let report = native.apply(black_box(&context), &mut summary);
                black_box((summary, report))
            });
        });
        group.bench_function(format!("starlark/{lines}"), |b| {
            b.iter(|| {
                let mut summary = InvocationSummary::default();
                let report = starlark.apply(black_box(&context), &mut summary);
                black_box((summary, report))
            });
        });
    }
    group.finish();
}

criterion_group!(benches, custom_reducers);
criterion_main!(benches);
