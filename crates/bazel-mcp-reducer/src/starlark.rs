use std::{
    collections::BTreeSet,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

use anyhow::{Context, anyhow, bail};
use bazel_mcp_types::{
    CoverageFile, CoverageSummary, Diagnostic, DiagnosticCategory, DiagnosticLocation,
    InvocationSummary, QueryRow, Severity, TargetCounts, TargetResult, TestCase, TestCounts,
    TestResult, TestStatus,
};
use regex::Regex;
use starlark::{
    PrintHandler,
    environment::{FrozenModule, Globals, GlobalsBuilder, Module},
    eval::Evaluator,
    starlark_module,
    syntax::{AstModule, Dialect, DialectTypes},
    values::{
        Heap, UnpackValue, Value, ValueIdentity,
        dict::{AllocDict, DictRef},
        list::{AllocList, ListRef},
        none::NoneOr,
        tuple::TupleRef,
    },
};

use crate::{
    CustomReducer, ReducerContext, ReducerError, ReducerEvent, ReducerEventKind, ReducerMode,
    ReducerPatch, ReducerPipeline, ReducerSelector,
};

pub const REDUCER_API_VERSION: u32 = 1;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StarlarkLimits {
    pub max_source_bytes: usize,
    pub max_input_bytes: usize,
    pub max_events: usize,
    pub max_output_bytes: usize,
    pub max_output_items: usize,
    pub max_ticks: u64,
    pub max_heap_bytes: usize,
    pub max_callstack_size: usize,
    pub timeout: Duration,
}

impl Default for StarlarkLimits {
    fn default() -> Self {
        Self {
            max_source_bytes: 256 * 1024,
            max_input_bytes: 1024 * 1024,
            max_events: 10_000,
            max_output_bytes: 64 * 1024,
            max_output_items: 1_000,
            max_ticks: 1_000_000,
            max_heap_bytes: 16 * 1024 * 1024,
            max_callstack_size: 100,
            timeout: Duration::from_millis(100),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StarlarkReducerConfig {
    pub files: Vec<PathBuf>,
    pub limits: StarlarkLimits,
}

pub fn load_starlark_reducers(
    config: &StarlarkReducerConfig,
) -> Result<ReducerPipeline, ReducerError> {
    validate_limits(&config.limits)?;
    let reducers = config
        .files
        .iter()
        .map(|path| {
            LoadedStarlarkReducer::load(path, config.limits.clone())
                .map(|reducer| Arc::new(reducer) as Arc<dyn CustomReducer>)
                .map_err(|error| {
                    ReducerError::new(format!(
                        "load Starlark reducer {}: {error:#}",
                        path.display()
                    ))
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    ReducerPipeline::new(reducers)
}

fn validate_limits(limits: &StarlarkLimits) -> Result<(), ReducerError> {
    if limits.max_source_bytes == 0
        || limits.max_input_bytes == 0
        || limits.max_events == 0
        || limits.max_output_bytes == 0
        || limits.max_output_items == 0
        || limits.max_ticks == 0
        || limits.max_heap_bytes == 0
        || limits.max_callstack_size == 0
        || limits.timeout.is_zero()
    {
        return Err(ReducerError::new(
            "all Starlark reducer resource limits must be greater than zero",
        ));
    }
    Ok(())
}

struct LoadedStarlarkReducer {
    name: String,
    priority: i32,
    mode: ReducerMode,
    selector: ReducerSelector,
    module: FrozenModule,
    limits: StarlarkLimits,
}

struct StarlarkPatch {
    headline: Option<String>,
    diagnostics: Vec<StarlarkDiagnostic>,
    suppress_builtin_diagnostics: bool,
}

struct StarlarkDiagnostic {
    severity: Severity,
    category: DiagnosticCategory,
    message: String,
    location: Option<StarlarkDiagnosticLocation>,
    target: Option<String>,
    action: Option<String>,
    repetition_count: u32,
}

struct StarlarkDiagnosticLocation {
    path: String,
    line: Option<u32>,
    column: Option<u32>,
}

impl From<StarlarkPatch> for ReducerPatch {
    fn from(patch: StarlarkPatch) -> Self {
        Self {
            headline: patch.headline,
            diagnostics: patch
                .diagnostics
                .into_iter()
                .map(|diagnostic| Diagnostic {
                    severity: diagnostic.severity,
                    category: diagnostic.category,
                    message: diagnostic.message,
                    location: diagnostic.location.map(|location| DiagnosticLocation {
                        path: location.path,
                        line: location.line,
                        column: location.column,
                    }),
                    target: diagnostic.target,
                    action: diagnostic.action,
                    repetition_count: diagnostic.repetition_count,
                })
                .collect(),
            suppress_builtin_diagnostics: patch.suppress_builtin_diagnostics,
        }
    }
}

impl LoadedStarlarkReducer {
    fn load(path: &Path, limits: StarlarkLimits) -> anyhow::Result<Self> {
        let mut source = String::new();
        File::open(path)
            .with_context(|| format!("open {}", path.display()))?
            .take(
                u64::try_from(limits.max_source_bytes)
                    .unwrap_or(u64::MAX)
                    .saturating_add(1),
            )
            .read_to_string(&mut source)
            .with_context(|| format!("read {} as UTF-8", path.display()))?;
        if source.len() > limits.max_source_bytes {
            bail!("source exceeds the {}-byte limit", limits.max_source_bytes);
        }
        let dialect = reducer_dialect();
        let ast = AstModule::parse(path.to_string_lossy().as_ref(), source, &dialect)
            .map_err(|error| anyhow!(error))?;
        let globals = reducer_globals();
        let module = Module::with_temp_heap(|module| {
            let no_print = NoPrint;
            let mut evaluator = Evaluator::new(&module);
            configure_evaluator(&mut evaluator, &limits)?;
            evaluator.set_print_handler(&no_print);
            evaluator.enable_static_typechecking(true);
            evaluator
                .eval_module(ast, globals)
                .map_err(|error| anyhow!(error))?;
            drop(evaluator);
            module.freeze().map_err(|error| anyhow!(error))
        })?;
        let api_version = required_i32(&module, "API_VERSION")?;
        if api_version != i32::try_from(REDUCER_API_VERSION).unwrap_or(1) {
            bail!("unsupported API_VERSION {api_version}; expected {REDUCER_API_VERSION}");
        }
        let name = required_string(&module, "NAME")?;
        if name.is_empty() || name.len() > 128 {
            bail!("NAME must contain between 1 and 128 bytes");
        }
        let priority = optional_i32(&module, "PRIORITY")?.unwrap_or(0);
        let mode = match optional_string(&module, "MODE")?
            .as_deref()
            .unwrap_or("augment")
        {
            "augment" => ReducerMode::Augment,
            "override_matching" => ReducerMode::OverrideMatching,
            value => bail!("MODE must be augment or override_matching, got {value:?}"),
        };
        let selector = ReducerSelector {
            commands: optional_string_set(&module, "COMMANDS")?,
            target_labels: optional_string_list(&module, "TARGET_LABELS")?,
            target_kinds: optional_string_set(&module, "TARGET_KINDS")?,
            action_types: optional_string_set(&module, "ACTION_TYPES")?,
        };
        if mode == ReducerMode::OverrideMatching && !selector.has_event_constraints() {
            bail!("override_matching reducers must declare an event selector");
        }
        let reduce = module
            .get("reduce")
            .context("required exported function reduce is missing")?;
        if reduce.value().get_type() != "function" {
            bail!("reduce must be a function");
        }
        Ok(Self {
            name,
            priority,
            mode,
            selector,
            module,
            limits,
        })
    }
}

impl CustomReducer for LoadedStarlarkReducer {
    fn name(&self) -> &str {
        &self.name
    }

    fn priority(&self) -> i32 {
        self.priority
    }

    fn mode(&self) -> ReducerMode {
        self.mode
    }

    fn selector(&self) -> &ReducerSelector {
        &self.selector
    }

    fn reduce(&self, context: &ReducerContext) -> Result<ReducerPatch, ReducerError> {
        Module::with_temp_heap(|module| {
            let function = self
                .module
                .get("reduce")
                .context("exported reduce function disappeared")?;
            let function = module.heap().access_owned_frozen_value(&function);
            let context = alloc_reducer_context(module.heap(), context);
            let no_print = NoPrint;
            let mut evaluator = Evaluator::new(&module);
            configure_evaluator(&mut evaluator, &self.limits)?;
            evaluator.set_print_handler(&no_print);
            let result = evaluator
                .eval_function(function, &[context], &[])
                .map_err(|error| anyhow!(error))?;
            if result.is_none() {
                return Ok(ReducerPatch::default());
            }
            let output_bytes = serialized_json_len(result).context("serialize reducer output")?;
            if output_bytes > self.limits.max_output_bytes {
                bail!(
                    "reducer output is {} bytes, exceeding the {}-byte limit",
                    output_bytes,
                    self.limits.max_output_bytes
                );
            }
            let patch = parse_starlark_patch(result, self.limits.max_output_items)
                .context("validate reducer patch")?;
            Ok(patch.into())
        })
        .map_err(|error: anyhow::Error| {
            ReducerError::new(format!("Starlark evaluation failed: {error:#}"))
        })
    }
}

fn serialized_json_len(value: Value<'_>) -> anyhow::Result<usize> {
    serialized_json_len_inner(value, &mut Vec::with_capacity(8))
}

fn serialized_json_len_inner<'v>(
    value: Value<'v>,
    ancestors: &mut Vec<ValueIdentity<'v>>,
) -> anyhow::Result<usize> {
    if value.is_none() {
        return Ok(4);
    }
    if let Some(value) = value.unpack_bool() {
        return Ok(if value { 4 } else { 5 });
    }
    if let Some(value) = value.unpack_str() {
        return Ok(serialized_json_string_len(value));
    }
    if value.get_type() == "int" {
        return Ok(match i64::unpack_value(value) {
            Ok(Some(value)) => signed_decimal_len(value),
            Ok(None) | Err(_) => value.to_str().len(),
        });
    }
    if value.get_type() == "float" {
        return Ok(value.to_json()?.len());
    }

    let identity = value.identity();
    if ancestors.contains(&identity) {
        bail!("cyclic {} value cannot be serialized", value.get_type());
    }
    ancestors.push(identity);
    let result = if let Some(items) = ListRef::from_value(value) {
        serialized_json_sequence_len(items.content(), ancestors)
    } else if let Some(items) = TupleRef::from_value(value) {
        serialized_json_sequence_len(items.content(), ancestors)
    } else if let Some(dict) = DictRef::from_value(value) {
        let mut bytes = 2_usize;
        for (index, (key, value)) in dict.iter().enumerate() {
            let key = key
                .unpack_str()
                .context("JSON object keys must be strings")?;
            if index != 0 {
                bytes = bytes.saturating_add(1);
            }
            bytes = bytes
                .saturating_add(serialized_json_string_len(key))
                .saturating_add(1)
                .saturating_add(serialized_json_len_inner(value, ancestors)?);
        }
        Ok(bytes)
    } else {
        bail!("{} values cannot be serialized as JSON", value.get_type())
    };
    ancestors.pop();
    result
}

fn serialized_json_sequence_len<'v>(
    values: &[Value<'v>],
    ancestors: &mut Vec<ValueIdentity<'v>>,
) -> anyhow::Result<usize> {
    let mut bytes = 2_usize;
    for (index, value) in values.iter().copied().enumerate() {
        if index != 0 {
            bytes = bytes.saturating_add(1);
        }
        bytes = bytes.saturating_add(serialized_json_len_inner(value, ancestors)?);
    }
    Ok(bytes)
}

fn serialized_json_string_len(value: &str) -> usize {
    value.bytes().fold(2_usize, |bytes, byte| {
        bytes.saturating_add(match byte {
            b'"' | b'\\' | b'\x08' | b'\t' | b'\n' | b'\x0c' | b'\r' => 2,
            0x00..=0x1f => 6,
            _ => 1,
        })
    })
}

const fn signed_decimal_len(value: i64) -> usize {
    let sign = if value < 0 { 1 } else { 0 };
    sign + unsigned_decimal_len(value.unsigned_abs())
}

const fn unsigned_decimal_len(mut value: u64) -> usize {
    let mut digits = 1;
    while value >= 10 {
        value /= 10;
        digits += 1;
    }
    digits
}

fn alloc_reducer_context<'v>(heap: Heap<'v>, context: &ReducerContext) -> Value<'v> {
    // Keep this in lockstep with ReducerContext's serialized API. The nested-context
    // contract test covers every top-level field and representative nested values.
    let arguments = heap.alloc(AllocList(context.arguments.iter().map(String::as_str)));
    let events = heap.alloc(AllocList(
        context
            .events
            .iter()
            .map(|event| alloc_reducer_event(heap, event)),
    ));
    let baseline = alloc_invocation_summary(heap, &context.baseline);
    heap.alloc(AllocDict([
        ("api_version", heap.alloc(context.api_version)),
        ("command", heap.alloc(context.command.as_str())),
        ("arguments", arguments),
        ("exit_code", alloc_optional_i32(heap, context.exit_code)),
        ("elapsed_ms", heap.alloc(context.elapsed_ms)),
        ("stdout", heap.alloc(context.stdout.as_str())),
        ("stderr", heap.alloc(context.stderr.as_str())),
        ("events", events),
        ("input_truncated", heap.alloc(context.input_truncated)),
        ("baseline", baseline),
    ]))
}

fn alloc_reducer_event<'v>(heap: Heap<'v>, event: &ReducerEvent) -> Value<'v> {
    heap.alloc(AllocDict([
        ("ordinal", heap.alloc(event.ordinal)),
        ("kind", heap.alloc(reducer_event_kind_name(event.kind))),
        ("label", alloc_optional_str(heap, event.label.as_deref())),
        (
            "target_kind",
            alloc_optional_str(heap, event.target_kind.as_deref()),
        ),
        (
            "action_type",
            alloc_optional_str(heap, event.action_type.as_deref()),
        ),
        ("success", alloc_optional_bool(heap, event.success)),
        ("exit_code", alloc_optional_i32(heap, event.exit_code)),
        (
            "message",
            alloc_optional_str(heap, event.message.as_deref()),
        ),
    ]))
}

fn alloc_invocation_summary<'v>(heap: Heap<'v>, summary: &InvocationSummary) -> Value<'v> {
    let targets = heap.alloc(AllocList(
        summary
            .targets
            .iter()
            .map(|target| alloc_target_result(heap, target)),
    ));
    let diagnostics = heap.alloc(AllocList(
        summary
            .diagnostics
            .iter()
            .map(|diagnostic| alloc_diagnostic(heap, diagnostic)),
    ));
    let tests = heap.alloc(AllocList(
        summary
            .tests
            .iter()
            .map(|test| alloc_test_result(heap, test)),
    ));
    let query_sample = heap.alloc(AllocList(
        summary
            .query_sample
            .iter()
            .map(|row| alloc_query_row(heap, row)),
    ));
    let coverage = summary
        .coverage
        .as_ref()
        .map_or_else(Value::new_none, |coverage| {
            alloc_coverage_summary(heap, coverage)
        });
    heap.alloc(AllocDict([
        ("success", heap.alloc(summary.success)),
        ("headline", heap.alloc(summary.headline.as_str())),
        ("targets", targets),
        (
            "target_counts",
            alloc_target_counts(heap, &summary.target_counts),
        ),
        ("diagnostics", diagnostics),
        ("tests", tests),
        ("test_counts", alloc_test_counts(heap, &summary.test_counts)),
        ("coverage", coverage),
        ("query_sample", query_sample),
        (
            "query_result_count",
            alloc_optional_u64(heap, summary.query_result_count),
        ),
        ("elapsed_ms", heap.alloc(summary.elapsed_ms)),
        ("truncated", heap.alloc(summary.truncated)),
        (
            "inspect_hint",
            alloc_optional_str(heap, summary.inspect_hint.as_deref()),
        ),
    ]))
}

fn alloc_target_result<'v>(heap: Heap<'v>, target: &TargetResult) -> Value<'v> {
    heap.alloc(AllocDict([
        ("label", heap.alloc(target.label.as_str())),
        ("success", heap.alloc(target.success)),
    ]))
}

fn alloc_target_counts<'v>(heap: Heap<'v>, counts: &TargetCounts) -> Value<'v> {
    heap.alloc(AllocDict([
        ("requested", heap.alloc(counts.requested)),
        ("succeeded", heap.alloc(counts.succeeded)),
        ("failed", heap.alloc(counts.failed)),
    ]))
}

fn alloc_diagnostic<'v>(heap: Heap<'v>, diagnostic: &Diagnostic) -> Value<'v> {
    let location = diagnostic
        .location
        .as_ref()
        .map_or_else(Value::new_none, |location| {
            alloc_diagnostic_location(heap, location)
        });
    alloc_diagnostic_fields(
        heap,
        severity_name(diagnostic.severity),
        category_name(diagnostic.category),
        &diagnostic.message,
        location,
        diagnostic.target.as_deref(),
        diagnostic.action.as_deref(),
        diagnostic.repetition_count,
    )
}

#[allow(clippy::too_many_arguments)]
fn alloc_diagnostic_fields<'v>(
    heap: Heap<'v>,
    severity: &str,
    category: &str,
    message: &str,
    location: Value<'v>,
    target: Option<&str>,
    action: Option<&str>,
    repetition_count: u32,
) -> Value<'v> {
    heap.alloc(AllocDict([
        ("severity", heap.alloc(severity)),
        ("category", heap.alloc(category)),
        ("message", heap.alloc(message)),
        ("location", location),
        ("target", alloc_optional_str(heap, target)),
        ("action", alloc_optional_str(heap, action)),
        ("repetition_count", heap.alloc(repetition_count)),
    ]))
}

fn alloc_diagnostic_location<'v>(heap: Heap<'v>, location: &DiagnosticLocation) -> Value<'v> {
    alloc_diagnostic_location_fields(heap, &location.path, location.line, location.column)
}

fn alloc_diagnostic_location_fields<'v>(
    heap: Heap<'v>,
    path: &str,
    line: Option<u32>,
    column: Option<u32>,
) -> Value<'v> {
    heap.alloc(AllocDict([
        ("path", heap.alloc(path)),
        ("line", alloc_optional_u32(heap, line)),
        ("column", alloc_optional_u32(heap, column)),
    ]))
}

fn alloc_test_result<'v>(heap: Heap<'v>, test: &TestResult) -> Value<'v> {
    let cases = heap.alloc(AllocList(
        test.cases.iter().map(|case| alloc_test_case(heap, case)),
    ));
    let mut fields = Vec::with_capacity(8);
    fields.extend([
        ("label", heap.alloc(test.label.as_str())),
        ("status", heap.alloc(test_status_name(test.status))),
        ("duration_ms", alloc_optional_u64(heap, test.duration_ms)),
        ("attempts", heap.alloc(test.attempts)),
        ("shard", alloc_optional_u32(heap, test.shard)),
        ("cases", cases),
        ("test_log_available", heap.alloc(test.test_log_available)),
    ]);
    if let Some(reason) = test.test_log_unavailable_reason.as_deref() {
        fields.push(("test_log_unavailable_reason", heap.alloc(reason)));
    }
    heap.alloc(AllocDict(fields))
}

fn alloc_test_case<'v>(heap: Heap<'v>, case: &TestCase) -> Value<'v> {
    heap.alloc(AllocDict([
        ("name", heap.alloc(case.name.as_str())),
        ("status", heap.alloc(test_status_name(case.status))),
        ("duration_ms", alloc_optional_u64(heap, case.duration_ms)),
        ("message", alloc_optional_str(heap, case.message.as_deref())),
    ]))
}

fn alloc_test_counts<'v>(heap: Heap<'v>, counts: &TestCounts) -> Value<'v> {
    heap.alloc(AllocDict([
        ("passed", heap.alloc(counts.passed)),
        ("failed", heap.alloc(counts.failed)),
        ("flaky", heap.alloc(counts.flaky)),
        ("skipped", heap.alloc(counts.skipped)),
        ("incomplete", heap.alloc(counts.incomplete)),
    ]))
}

fn alloc_coverage_summary<'v>(heap: Heap<'v>, coverage: &CoverageSummary) -> Value<'v> {
    let files = heap.alloc(AllocList(
        coverage
            .files
            .iter()
            .map(|file| alloc_coverage_file(heap, file)),
    ));
    heap.alloc(AllocDict([
        ("lines_found", heap.alloc(coverage.lines_found)),
        ("lines_hit", heap.alloc(coverage.lines_hit)),
        ("coverage_percent", heap.alloc(coverage.coverage_percent)),
        ("files", files),
    ]))
}

fn alloc_coverage_file<'v>(heap: Heap<'v>, file: &CoverageFile) -> Value<'v> {
    heap.alloc(AllocDict([
        ("path", heap.alloc(file.path.as_str())),
        ("lines_found", heap.alloc(file.lines_found)),
        ("lines_hit", heap.alloc(file.lines_hit)),
        ("coverage_percent", heap.alloc(file.coverage_percent)),
    ]))
}

fn alloc_query_row<'v>(heap: Heap<'v>, row: &QueryRow) -> Value<'v> {
    heap.alloc(AllocDict([
        ("ordinal", heap.alloc(row.ordinal)),
        ("value", heap.alloc(row.value.as_str())),
    ]))
}

fn alloc_optional_str<'v>(heap: Heap<'v>, value: Option<&str>) -> Value<'v> {
    value.map_or_else(Value::new_none, |value| heap.alloc(value))
}

fn alloc_optional_bool<'v>(heap: Heap<'v>, value: Option<bool>) -> Value<'v> {
    value.map_or_else(Value::new_none, |value| heap.alloc(value))
}

fn alloc_optional_i32<'v>(heap: Heap<'v>, value: Option<i32>) -> Value<'v> {
    value.map_or_else(Value::new_none, |value| heap.alloc(value))
}

fn alloc_optional_u32<'v>(heap: Heap<'v>, value: Option<u32>) -> Value<'v> {
    value.map_or_else(Value::new_none, |value| heap.alloc(value))
}

fn alloc_optional_u64<'v>(heap: Heap<'v>, value: Option<u64>) -> Value<'v> {
    value.map_or_else(Value::new_none, |value| heap.alloc(value))
}

const fn reducer_event_kind_name(kind: ReducerEventKind) -> &'static str {
    match kind {
        ReducerEventKind::Aborted => "aborted",
        ReducerEventKind::Action => "action",
        ReducerEventKind::Target => "target",
        ReducerEventKind::TestSummary => "test_summary",
    }
}

const fn severity_name(severity: Severity) -> &'static str {
    match severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Note => "note",
    }
}

const fn category_name(category: DiagnosticCategory) -> &'static str {
    match category {
        DiagnosticCategory::Workspace => "workspace",
        DiagnosticCategory::Loading => "loading",
        DiagnosticCategory::Analysis => "analysis",
        DiagnosticCategory::Visibility => "visibility",
        DiagnosticCategory::Action => "action",
        DiagnosticCategory::Compilation => "compilation",
        DiagnosticCategory::Test => "test",
        DiagnosticCategory::Bazel => "bazel",
        DiagnosticCategory::Unknown => "unknown",
    }
}

const fn test_status_name(status: TestStatus) -> &'static str {
    match status {
        TestStatus::Passed => "passed",
        TestStatus::Failed => "failed",
        TestStatus::Flaky => "flaky",
        TestStatus::Skipped => "skipped",
        TestStatus::TimedOut => "timed_out",
        TestStatus::Incomplete => "incomplete",
        TestStatus::Remote => "remote",
    }
}

fn parse_starlark_patch(
    value: Value<'_>,
    max_output_items: usize,
) -> anyhow::Result<StarlarkPatch> {
    let patch = required_dict(value, "reducer patch")?;
    validate_fields(
        &patch,
        &["headline", "diagnostics", "suppress_builtin_diagnostics"],
        "reducer patch",
    )?;
    let diagnostics_value = required_field(&patch, "diagnostics", "reducer patch")?;
    let diagnostic_values = sequence_items(diagnostics_value, "diagnostics")?;
    if diagnostic_values.len() > max_output_items {
        bail!(
            "reducer returned {} diagnostics, exceeding the {}-item limit",
            diagnostic_values.len(),
            max_output_items
        );
    }
    let diagnostics = diagnostic_values
        .iter()
        .copied()
        .enumerate()
        .map(|(index, value)| {
            parse_starlark_diagnostic(value)
                .with_context(|| format!("validate diagnostic at index {index}"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(StarlarkPatch {
        headline: optional_string_field(&patch, "headline", "reducer patch")?,
        diagnostics,
        suppress_builtin_diagnostics: required_bool_field(
            &patch,
            "suppress_builtin_diagnostics",
            "reducer patch",
        )?,
    })
}

fn parse_starlark_diagnostic(value: Value<'_>) -> anyhow::Result<StarlarkDiagnostic> {
    let diagnostic = required_dict(value, "diagnostic")?;
    validate_fields(
        &diagnostic,
        &[
            "severity",
            "category",
            "message",
            "location",
            "target",
            "action",
            "repetition_count",
        ],
        "diagnostic",
    )?;
    let severity = match required_str_field(&diagnostic, "severity", "diagnostic")? {
        "error" => Severity::Error,
        "warning" => Severity::Warning,
        "note" => Severity::Note,
        value => bail!("invalid diagnostic severity {value:?}"),
    };
    let category = match required_str_field(&diagnostic, "category", "diagnostic")? {
        "workspace" => DiagnosticCategory::Workspace,
        "loading" => DiagnosticCategory::Loading,
        "analysis" => DiagnosticCategory::Analysis,
        "visibility" => DiagnosticCategory::Visibility,
        "action" => DiagnosticCategory::Action,
        "compilation" => DiagnosticCategory::Compilation,
        "test" => DiagnosticCategory::Test,
        "bazel" => DiagnosticCategory::Bazel,
        "unknown" => DiagnosticCategory::Unknown,
        value => bail!("invalid diagnostic category {value:?}"),
    };
    Ok(StarlarkDiagnostic {
        severity,
        category,
        message: required_str_field(&diagnostic, "message", "diagnostic")?.to_owned(),
        location: optional_location_field(&diagnostic)?,
        target: optional_string_field(&diagnostic, "target", "diagnostic")?,
        action: optional_string_field(&diagnostic, "action", "diagnostic")?,
        repetition_count: required_u32_field(&diagnostic, "repetition_count", "diagnostic")?,
    })
}

fn optional_location_field(
    diagnostic: &DictRef<'_>,
) -> anyhow::Result<Option<StarlarkDiagnosticLocation>> {
    let Some(value) = diagnostic.get_str("location") else {
        return Ok(None);
    };
    if value.is_none() {
        return Ok(None);
    }
    let location = required_dict(value, "diagnostic location")?;
    validate_fields(
        &location,
        &["path", "line", "column"],
        "diagnostic location",
    )?;
    Ok(Some(StarlarkDiagnosticLocation {
        path: required_str_field(&location, "path", "diagnostic location")?.to_owned(),
        line: optional_u32_field(&location, "line", "diagnostic location")?,
        column: optional_u32_field(&location, "column", "diagnostic location")?,
    }))
}

fn required_dict<'v>(value: Value<'v>, name: &str) -> anyhow::Result<DictRef<'v>> {
    DictRef::from_value(value).with_context(|| format!("{name} must be a dict"))
}

fn sequence_items<'v>(value: Value<'v>, name: &str) -> anyhow::Result<&'v [Value<'v>]> {
    if let Some(list) = ListRef::from_value(value) {
        Ok(list.content())
    } else if let Some(tuple) = TupleRef::from_value(value) {
        Ok(tuple.content())
    } else {
        bail!("{name} must be a list or tuple")
    }
}

fn validate_fields(dict: &DictRef<'_>, allowed: &[&str], name: &str) -> anyhow::Result<()> {
    for (key, _) in dict.iter() {
        let key = key
            .unpack_str()
            .with_context(|| format!("{name} field names must be strings"))?;
        if !allowed.contains(&key) {
            bail!("unknown field {key:?} in {name}");
        }
    }
    Ok(())
}

fn required_field<'v>(dict: &DictRef<'v>, field: &str, name: &str) -> anyhow::Result<Value<'v>> {
    dict.get_str(field)
        .with_context(|| format!("required field {field:?} is missing from {name}"))
}

fn required_str_field<'v>(dict: &DictRef<'v>, field: &str, name: &str) -> anyhow::Result<&'v str> {
    required_field(dict, field, name)?
        .unpack_str()
        .with_context(|| format!("{name} field {field:?} must be a string"))
}

fn optional_string_field(
    dict: &DictRef<'_>,
    field: &str,
    name: &str,
) -> anyhow::Result<Option<String>> {
    let Some(value) = dict.get_str(field) else {
        return Ok(None);
    };
    if value.is_none() {
        Ok(None)
    } else {
        value
            .unpack_str()
            .map(|value| Some(value.to_owned()))
            .with_context(|| format!("{name} field {field:?} must be a string or None"))
    }
}

fn required_bool_field(dict: &DictRef<'_>, field: &str, name: &str) -> anyhow::Result<bool> {
    required_field(dict, field, name)?
        .unpack_bool()
        .with_context(|| format!("{name} field {field:?} must be a bool"))
}

fn required_u32_field(dict: &DictRef<'_>, field: &str, name: &str) -> anyhow::Result<u32> {
    u32::unpack_value_err(required_field(dict, field, name)?)
        .map_err(|error| anyhow!(error))
        .with_context(|| format!("{name} field {field:?} must be an unsigned 32-bit integer"))
}

fn optional_u32_field(dict: &DictRef<'_>, field: &str, name: &str) -> anyhow::Result<Option<u32>> {
    let Some(value) = dict.get_str(field) else {
        return Ok(None);
    };
    if value.is_none() {
        Ok(None)
    } else {
        u32::unpack_value_err(value)
            .map(Some)
            .map_err(|error| anyhow!(error))
            .with_context(|| {
                format!("{name} field {field:?} must be an unsigned 32-bit integer or None")
            })
    }
}

fn reducer_dialect() -> Dialect {
    Dialect {
        enable_load: false,
        enable_types: DialectTypes::Enable,
        ..Dialect::Standard
    }
}

fn configure_evaluator(
    evaluator: &mut Evaluator<'_, '_, '_>,
    limits: &StarlarkLimits,
) -> anyhow::Result<()> {
    evaluator
        .set_max_callstack_size(limits.max_callstack_size)
        .map_err(|error| anyhow!(error))?;
    evaluator
        .set_max_heap_size(limits.max_heap_bytes)
        .map_err(|error| anyhow!(error))?;
    evaluator
        .set_max_tick_count(limits.max_ticks)
        .map_err(|error| anyhow!(error))?;
    let started = Instant::now();
    let timeout = limits.timeout;
    evaluator.set_check_cancelled(Box::new(move || started.elapsed() >= timeout));
    Ok(())
}

struct NoPrint;

impl PrintHandler for NoPrint {
    fn println(&self, _text: &str) -> starlark::Result<()> {
        Err(starlark::Error::new_other(anyhow!(
            "print is disabled in custom reducers"
        )))
    }
}

fn reducer_globals() -> &'static Globals {
    static GLOBALS: OnceLock<Globals> = OnceLock::new();
    GLOBALS.get_or_init(|| GlobalsBuilder::standard().with(reducer_api).build())
}

#[allow(clippy::too_many_arguments)]
#[starlark_module]
fn reducer_api(builder: &mut GlobalsBuilder) {
    #[allow(clippy::too_many_arguments)]
    fn diagnostic<'v>(
        message: &str,
        #[starlark(require = named, default = "error")] severity: &str,
        #[starlark(require = named, default = "unknown")] category: &str,
        #[starlark(require = named, default = NoneOr::None)] target: NoneOr<&str>,
        #[starlark(require = named, default = NoneOr::None)] action: NoneOr<&str>,
        #[starlark(require = named, default = NoneOr::None)] path: NoneOr<&str>,
        #[starlark(require = named, default = NoneOr::None)] line: NoneOr<i32>,
        #[starlark(require = named, default = NoneOr::None)] column: NoneOr<i32>,
        #[starlark(require = named, default = 1)] repetition_count: i32,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        validate_severity(severity)?;
        validate_category(category)?;
        if repetition_count <= 0 {
            bail!("repetition_count must be greater than zero");
        }
        let path = path.into_option();
        if path.is_none() && (!line.is_none() || !column.is_none()) {
            bail!("line and column require path");
        }
        let line = positive_optional(line.into_option(), "line")?;
        let column = positive_optional(column.into_option(), "column")?;
        let location = path.map_or_else(Value::new_none, |path| {
            alloc_diagnostic_location_fields(heap, path, line, column)
        });
        Ok(alloc_diagnostic_fields(
            heap,
            severity,
            category,
            message,
            location,
            target.into_option(),
            action.into_option(),
            u32::try_from(repetition_count).unwrap_or(1),
        ))
    }

    fn patch<'v>(
        diagnostics: Value<'v>,
        #[starlark(require = named, default = NoneOr::None)] headline: NoneOr<&str>,
        #[starlark(require = named, default = false)] suppress_builtin_diagnostics: bool,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        sequence_items(diagnostics, "diagnostics")?;
        Ok(heap.alloc(AllocDict([
            ("headline", alloc_optional_str(heap, headline.into_option())),
            ("diagnostics", diagnostics),
            (
                "suppress_builtin_diagnostics",
                heap.alloc(suppress_builtin_diagnostics),
            ),
        ])))
    }

    fn regex_diagnostics<'v>(
        text: &str,
        pattern: &str,
        #[starlark(require = named, default = "error")] severity: &str,
        #[starlark(require = named, default = "unknown")] category: &str,
        #[starlark(require = named, default = 50)] max_matches: i32,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        validate_severity(severity)?;
        validate_category(category)?;
        if !(1..=1000).contains(&max_matches) {
            bail!("max_matches must be between 1 and 1000");
        }
        let pattern = Regex::new(pattern).context("compile reducer regular expression")?;
        let diagnostics = pattern
            .captures_iter(text)
            .take(usize::try_from(max_matches).unwrap_or(1000))
            .map(|captures| {
                let capture = |name| captures.name(name).map(|value| value.as_str());
                let parse_position = |name| {
                    capture(name)
                        .map(str::parse::<u32>)
                        .transpose()
                        .with_context(|| format!("capture {name:?} is not a positive integer"))
                };
                let path = capture("path");
                let location = if let Some(path) = path {
                    alloc_diagnostic_location_fields(
                        heap,
                        path,
                        parse_position("line")?,
                        parse_position("column")?,
                    )
                } else {
                    Value::new_none()
                };
                Ok(alloc_diagnostic_fields(
                    heap,
                    severity,
                    category,
                    capture("message")
                        .unwrap_or_else(|| captures.get(0).map_or("", |value| value.as_str())),
                    location,
                    capture("target"),
                    capture("action"),
                    1,
                ))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(heap.alloc(AllocList(diagnostics)))
    }
}

fn positive_optional(value: Option<i32>, name: &str) -> anyhow::Result<Option<u32>> {
    value
        .map(|value| u32::try_from(value).with_context(|| format!("{name} must not be negative")))
        .transpose()
}

fn validate_severity(value: &str) -> anyhow::Result<()> {
    if matches!(value, "error" | "warning" | "note") {
        Ok(())
    } else {
        bail!("invalid diagnostic severity {value:?}")
    }
}

fn validate_category(value: &str) -> anyhow::Result<()> {
    if matches!(
        value,
        "workspace"
            | "loading"
            | "analysis"
            | "visibility"
            | "action"
            | "compilation"
            | "test"
            | "bazel"
            | "unknown"
    ) {
        Ok(())
    } else {
        bail!("invalid diagnostic category {value:?}")
    }
}

fn required_string(module: &FrozenModule, name: &str) -> anyhow::Result<String> {
    optional_string(module, name)?
        .with_context(|| format!("required exported string {name} is missing"))
}

fn optional_string(module: &FrozenModule, name: &str) -> anyhow::Result<Option<String>> {
    module
        .get_option(name)?
        .map(|value| {
            value
                .unpack_str()
                .map(str::to_owned)
                .with_context(|| format!("{name} must be a string"))
        })
        .transpose()
}

fn required_i32(module: &FrozenModule, name: &str) -> anyhow::Result<i32> {
    optional_i32(module, name)?
        .with_context(|| format!("required exported integer {name} is missing"))
}

fn optional_i32(module: &FrozenModule, name: &str) -> anyhow::Result<Option<i32>> {
    module
        .get_option(name)?
        .map(|value| {
            value
                .unpack_i32()
                .with_context(|| format!("{name} must be a 32-bit integer"))
        })
        .transpose()
}

fn optional_string_list(module: &FrozenModule, name: &str) -> anyhow::Result<Vec<String>> {
    let Some(value) = module.get_option(name)? else {
        return Ok(Vec::new());
    };
    sequence_items(value.value(), name)?
        .iter()
        .map(|value| {
            value
                .unpack_str()
                .map(str::to_owned)
                .with_context(|| format!("{name} must be a list of strings"))
        })
        .collect()
}

fn optional_string_set(module: &FrozenModule, name: &str) -> anyhow::Result<BTreeSet<String>> {
    Ok(optional_string_list(module, name)?.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use bazel_mcp_types::InvocationSummary;
    use tempfile::tempdir;

    use super::*;

    fn context(stderr: &str) -> ReducerContext {
        ReducerContext {
            api_version: REDUCER_API_VERSION,
            command: "build".to_owned(),
            arguments: vec!["//swift:lib".to_owned()],
            exit_code: Some(1),
            elapsed_ms: 10,
            stdout: String::new(),
            stderr: stderr.to_owned(),
            events: Vec::new(),
            input_truncated: false,
            baseline: InvocationSummary::default(),
        }
    }

    #[test]
    fn caches_reducer_globals() {
        assert!(std::ptr::eq(reducer_globals(), reducer_globals()));
    }

    #[test]
    fn direct_json_size_matches_starlark_serialization() {
        Heap::temp(|heap| {
            let items = heap.alloc(AllocList([
                heap.alloc("quote \" slash \\ newline\n snowman ☃"),
                heap.alloc(-42_i32),
                heap.alloc(u64::MAX),
                heap.alloc(12.5_f64),
                Value::new_none(),
            ]));
            let value = heap.alloc(AllocDict([("items", items), ("enabled", heap.alloc(true))]));
            assert_eq!(
                serialized_json_len(value).unwrap(),
                value.to_json().unwrap().len()
            );
        });
    }

    #[test]
    fn loads_and_applies_a_typed_starlark_patch() {
        let root = tempdir().unwrap();
        let path = root.path().join("swift.star");
        fs::write(
            &path,
            r#"
API_VERSION = 1
NAME = "swift"
COMMANDS = ["build"]

def reduce(ctx):
    diagnostics = regex_diagnostics(
        ctx["stderr"],
        r"(?P<path>[^:]+):(?P<line>[0-9]+):(?P<column>[0-9]+): error: (?P<message>.+)",
        category = "compilation",
    )
    return patch(diagnostics, headline = "Swift compilation failed")
"#,
        )
        .unwrap();
        let pipeline = load_starlark_reducers(&StarlarkReducerConfig {
            files: vec![path],
            limits: StarlarkLimits::default(),
        })
        .unwrap();
        let context = context("Sources/App.swift:12:7: error: missing value");
        let mut summary = InvocationSummary::default();
        let report = pipeline.apply(&context, &mut summary);
        assert_eq!(report.applied, vec!["swift"]);
        assert!(report.failures.is_empty());
        assert!(report.headline_applied);
        assert_eq!(summary.headline, "Swift compilation failed");
        assert_eq!(summary.diagnostics.len(), 1);
        assert_eq!(summary.diagnostics[0].message, "missing value");
        assert_eq!(
            summary.diagnostics[0].location.as_ref().unwrap().path,
            "Sources/App.swift"
        );
    }

    #[test]
    fn exposes_the_complete_nested_context_without_json_round_trips() {
        let root = tempdir().unwrap();
        let path = root.path().join("context.star");
        fs::write(
            &path,
            r#"
API_VERSION = 1
NAME = "context"

def reduce(ctx):
    baseline = ctx["baseline"]
    source = baseline["diagnostics"][0]
    event = ctx["events"][0]
    if (
        ctx["arguments"][0] != "//app:target" or
        ctx["exit_code"] != 1 or
        ctx["elapsed_ms"] != 42 or
        ctx["stdout"] != "stdout" or
        event["kind"] != "action" or
        event["success"] != False or
        baseline["targets"][0]["success"] != False or
        baseline["target_counts"]["requested"] != 3 or
        baseline["tests"][0]["status"] != "flaky" or
        "test_log_unavailable_reason" in baseline["tests"][0] or
        baseline["test_counts"]["flaky"] != 1 or
        baseline["coverage"]["coverage_percent"] != 75.0 or
        baseline["query_sample"][0]["ordinal"] != 7 or
        baseline["query_result_count"] != 9 or
        baseline["inspect_hint"] != "inspect hint"
    ):
        return patch([diagnostic("context mismatch")])
    return patch([
        diagnostic(
            source["message"],
            severity = source["severity"],
            category = source["category"],
            target = event["label"],
            action = event["action_type"],
            path = baseline["coverage"]["files"][0]["path"],
            line = source["location"]["line"],
            column = source["location"]["column"],
            repetition_count = source["repetition_count"],
        ),
    ], headline = baseline["headline"], suppress_builtin_diagnostics = ctx["input_truncated"])
"#,
        )
        .unwrap();
        let pipeline = load_starlark_reducers(&StarlarkReducerConfig {
            files: vec![path],
            limits: StarlarkLimits::default(),
        })
        .unwrap();
        let context = ReducerContext {
            api_version: REDUCER_API_VERSION,
            command: "build".to_owned(),
            arguments: vec!["//app:target".to_owned()],
            exit_code: Some(1),
            elapsed_ms: 42,
            stdout: "stdout".to_owned(),
            stderr: "stderr".to_owned(),
            events: vec![ReducerEvent {
                ordinal: 4,
                kind: ReducerEventKind::Action,
                label: Some("//app:target".to_owned()),
                target_kind: Some("cc_library".to_owned()),
                action_type: Some("CppCompile".to_owned()),
                success: Some(false),
                exit_code: Some(1),
                message: Some("action failed".to_owned()),
            }],
            input_truncated: true,
            baseline: InvocationSummary {
                success: false,
                headline: "native headline".to_owned(),
                targets: vec![TargetResult {
                    label: "//app:target".to_owned(),
                    success: false,
                }],
                target_counts: TargetCounts {
                    requested: 3,
                    succeeded: 2,
                    failed: 1,
                },
                diagnostics: vec![Diagnostic {
                    severity: Severity::Warning,
                    category: DiagnosticCategory::Test,
                    message: "nested message".to_owned(),
                    location: Some(DiagnosticLocation {
                        path: "ignored/by/script".to_owned(),
                        line: Some(12),
                        column: Some(7),
                    }),
                    target: None,
                    action: None,
                    repetition_count: 2,
                }],
                tests: vec![TestResult {
                    label: "//app:test".to_owned(),
                    status: TestStatus::Flaky,
                    duration_ms: Some(10),
                    attempts: 2,
                    shard: Some(1),
                    cases: vec![TestCase {
                        name: "case".to_owned(),
                        status: TestStatus::Passed,
                        duration_ms: Some(5),
                        message: None,
                    }],
                    test_log_available: true,
                    test_log_unavailable_reason: None,
                }],
                test_counts: TestCounts {
                    flaky: 1,
                    ..TestCounts::default()
                },
                coverage: Some(CoverageSummary {
                    lines_found: 4,
                    lines_hit: 3,
                    coverage_percent: 75.0,
                    files: vec![CoverageFile {
                        path: "src/lib.rs".to_owned(),
                        lines_found: 4,
                        lines_hit: 3,
                        coverage_percent: 75.0,
                    }],
                }),
                query_sample: vec![QueryRow {
                    ordinal: 7,
                    value: "//app:target".to_owned(),
                }],
                query_result_count: Some(9),
                elapsed_ms: 41,
                truncated: false,
                inspect_hint: Some("inspect hint".to_owned()),
            },
        };
        let mut summary = InvocationSummary::default();
        let report = pipeline.apply(&context, &mut summary);
        assert!(report.failures.is_empty());
        assert_eq!(summary.headline, "native headline");
        assert_eq!(summary.diagnostics.len(), 1);
        assert_eq!(summary.diagnostics[0].message, "nested message");
        assert_eq!(summary.diagnostics[0].severity, Severity::Warning);
        assert_eq!(summary.diagnostics[0].category, DiagnosticCategory::Test);
        assert_eq!(
            summary.diagnostics[0].target.as_deref(),
            Some("//app:target")
        );
        assert_eq!(summary.diagnostics[0].action.as_deref(), Some("CppCompile"));
        assert_eq!(summary.diagnostics[0].repetition_count, 2);
        assert_eq!(
            summary.diagnostics[0].location,
            Some(DiagnosticLocation {
                path: "src/lib.rs".to_owned(),
                line: Some(12),
                column: Some(7),
            })
        );
    }

    #[test]
    fn accepts_tuple_diagnostics_and_missing_optional_patch_fields() {
        let root = tempdir().unwrap();
        let path = root.path().join("tuple.star");
        fs::write(
            &path,
            r#"
API_VERSION = 1
NAME = "tuple"

def reduce(ctx):
    return {
        "diagnostics": ({
            "severity": "note",
            "category": "bazel",
            "message": "tuple diagnostic",
            "repetition_count": 1,
        },),
        "suppress_builtin_diagnostics": False,
    }
"#,
        )
        .unwrap();
        let pipeline = load_starlark_reducers(&StarlarkReducerConfig {
            files: vec![path],
            limits: StarlarkLimits::default(),
        })
        .unwrap();
        let mut summary = InvocationSummary::default();
        let report = pipeline.apply(&context(""), &mut summary);
        assert!(report.failures.is_empty());
        assert_eq!(summary.diagnostics.len(), 1);
        assert_eq!(summary.diagnostics[0].message, "tuple diagnostic");
        assert!(summary.diagnostics[0].location.is_none());
        assert!(summary.diagnostics[0].target.is_none());
        assert!(summary.diagnostics[0].action.is_none());
    }

    #[test]
    fn runtime_failure_keeps_the_native_summary() {
        let root = tempdir().unwrap();
        let path = root.path().join("broken.star");
        fs::write(
            &path,
            r#"
API_VERSION = 1
NAME = "broken"
COMMANDS = ["build"]

def reduce(ctx):
    return 1 // 0
"#,
        )
        .unwrap();
        let pipeline = load_starlark_reducers(&StarlarkReducerConfig {
            files: vec![path],
            limits: StarlarkLimits::default(),
        })
        .unwrap();
        let mut summary = InvocationSummary {
            headline: "native".to_owned(),
            ..InvocationSummary::default()
        };
        let report = pipeline.apply(&context(""), &mut summary);
        assert!(report.applied.is_empty());
        assert_eq!(report.failures.len(), 1);
        assert_eq!(summary.headline, "native");
    }

    #[test]
    fn none_is_a_no_op_instead_of_an_applied_patch() {
        let root = tempdir().unwrap();
        let path = root.path().join("no-op.star");
        fs::write(
            &path,
            r#"
API_VERSION = 1
NAME = "no-op"
COMMANDS = ["build"]

def reduce(ctx):
    return None
"#,
        )
        .unwrap();
        let pipeline = load_starlark_reducers(&StarlarkReducerConfig {
            files: vec![path],
            limits: StarlarkLimits::default(),
        })
        .unwrap();
        let mut summary = InvocationSummary::default();
        let report = pipeline.apply(&context(""), &mut summary);
        assert!(report.applied.is_empty());
        assert!(!report.headline_applied);
    }

    #[test]
    fn load_and_print_are_disabled() {
        let root = tempdir().unwrap();
        for (name, source) in [
            (
                "load.star",
                "load(\"other.star\", \"value\")\nAPI_VERSION = 1\nNAME = \"x\"\ndef reduce(ctx): return None\n",
            ),
            (
                "print.star",
                "print(\"secret\")\nAPI_VERSION = 1\nNAME = \"x\"\ndef reduce(ctx): return None\n",
            ),
        ] {
            let path = root.path().join(name);
            fs::write(&path, source).unwrap();
            assert!(
                load_starlark_reducers(&StarlarkReducerConfig {
                    files: vec![path],
                    limits: StarlarkLimits::default(),
                })
                .is_err()
            );
        }
    }

    #[test]
    fn rejects_invalid_exports_and_nested_patch_fields() {
        let root = tempdir().unwrap();
        let invalid_function = root.path().join("invalid-function.star");
        fs::write(
            &invalid_function,
            "API_VERSION = 1\nNAME = \"invalid\"\nreduce = 42\n",
        )
        .unwrap();
        assert!(
            load_starlark_reducers(&StarlarkReducerConfig {
                files: vec![invalid_function],
                limits: StarlarkLimits::default(),
            })
            .is_err()
        );

        let invalid_patch = root.path().join("invalid-patch.star");
        fs::write(
            &invalid_patch,
            r#"
API_VERSION = 1
NAME = "invalid-patch"

def reduce(ctx):
    return {
        "headline": None,
        "diagnostics": [{
            "severity": "error",
            "category": "unknown",
            "message": "failure",
            "location": None,
            "target": None,
            "action": None,
            "repetition_count": 1,
            "unexpected": True,
        }],
        "suppress_builtin_diagnostics": False,
    }
"#,
        )
        .unwrap();
        let pipeline = load_starlark_reducers(&StarlarkReducerConfig {
            files: vec![invalid_patch],
            limits: StarlarkLimits::default(),
        })
        .unwrap();
        let mut summary = InvocationSummary::default();
        let report = pipeline.apply(&context(""), &mut summary);
        assert_eq!(report.failures.len(), 1);
        assert!(summary.diagnostics.is_empty());
    }
}
