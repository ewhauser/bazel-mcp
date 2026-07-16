use std::{
    collections::BTreeSet,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, anyhow, bail};
use bazel_mcp_types::{Diagnostic, DiagnosticCategory, DiagnosticLocation, Severity};
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value as JsonValue, json};
use starlark::{
    PrintHandler,
    environment::{FrozenModule, Globals, GlobalsBuilder, Module},
    eval::Evaluator,
    starlark_module,
    syntax::{AstModule, Dialect, DialectTypes},
    values::{Heap, Value, none::NoneOr},
};

use crate::{
    CustomReducer, ReducerContext, ReducerError, ReducerMode, ReducerPatch, ReducerPipeline,
    ReducerSelector,
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StarlarkPatch {
    headline: Option<String>,
    diagnostics: Vec<StarlarkDiagnostic>,
    suppress_builtin_diagnostics: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StarlarkDiagnostic {
    severity: Severity,
    category: DiagnosticCategory,
    message: String,
    location: Option<StarlarkDiagnosticLocation>,
    target: Option<String>,
    action: Option<String>,
    repetition_count: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
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
                .eval_module(ast, &globals)
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
            let context = serde_json::to_value(context).context("serialize reducer context")?;
            let context = module.heap().alloc(context);
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
            let json = result.to_json().context("serialize reducer output")?;
            if json.len() > self.limits.max_output_bytes {
                bail!(
                    "reducer output is {} bytes, exceeding the {}-byte limit",
                    json.len(),
                    self.limits.max_output_bytes
                );
            }
            let patch: StarlarkPatch =
                serde_json::from_str(&json).context("validate reducer patch")?;
            if patch.diagnostics.len() > self.limits.max_output_items {
                bail!(
                    "reducer returned {} diagnostics, exceeding the {}-item limit",
                    patch.diagnostics.len(),
                    self.limits.max_output_items
                );
            }
            Ok(patch.into())
        })
        .map_err(|error: anyhow::Error| {
            ReducerError::new(format!("Starlark evaluation failed: {error:#}"))
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

fn reducer_globals() -> Globals {
    GlobalsBuilder::standard().with(reducer_api).build()
}

#[allow(clippy::too_many_arguments)]
#[starlark_module]
fn reducer_api(builder: &mut GlobalsBuilder) {
    #[allow(clippy::too_many_arguments)]
    fn diagnostic<'v>(
        message: &str,
        #[starlark(require = named, default = "error")] severity: &str,
        #[starlark(require = named, default = "unknown")] category: &str,
        #[starlark(require = named, default = NoneOr::None)] target: NoneOr<String>,
        #[starlark(require = named, default = NoneOr::None)] action: NoneOr<String>,
        #[starlark(require = named, default = NoneOr::None)] path: NoneOr<String>,
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
        Ok(heap.alloc(json!({
            "severity": severity,
            "category": category,
            "message": message,
            "location": path.map(|path| json!({
                "path": path,
                "line": line,
                "column": column,
            })),
            "target": target.into_option(),
            "action": action.into_option(),
            "repetition_count": repetition_count,
        })))
    }

    fn patch<'v>(
        diagnostics: Value<'v>,
        #[starlark(require = named, default = NoneOr::None)] headline: NoneOr<String>,
        #[starlark(require = named, default = false)] suppress_builtin_diagnostics: bool,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let diagnostics = diagnostics.to_json_value()?;
        if !diagnostics.is_array() {
            bail!("diagnostics must be a list");
        }
        Ok(heap.alloc(json!({
            "headline": headline.into_option(),
            "diagnostics": diagnostics,
            "suppress_builtin_diagnostics": suppress_builtin_diagnostics,
        })))
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
                let location = path
                    .map(|path| {
                        Ok::<JsonValue, anyhow::Error>(json!({
                            "path": path,
                            "line": parse_position("line")?,
                            "column": parse_position("column")?,
                        }))
                    })
                    .transpose()?;
                Ok(json!({
                    "severity": severity,
                    "category": category,
                    "message": capture("message").unwrap_or_else(|| captures.get(0).map_or("", |value| value.as_str())),
                    "location": location,
                    "target": capture("target"),
                    "action": capture("action"),
                    "repetition_count": 1,
                }))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(heap.alloc(JsonValue::Array(diagnostics)))
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
    serde_json::from_value(value.value().to_json_value()?)
        .with_context(|| format!("{name} must be a list of strings"))
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
