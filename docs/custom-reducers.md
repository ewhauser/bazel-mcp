# Custom reducers

`bazel-mcp` keeps its built-in Rust reducers and optionally runs explicitly
configured Starlark reducers afterward. This is intended for organization- or
rule-specific diagnostics that the built-in reducers cannot recognize.

The extension boundary is deliberately narrow. A custom reducer can add a
headline and typed diagnostics, or replace built-in diagnostics associated with
selected target/action evidence. It cannot change whether Bazel succeeded,
invocation identity, termination, metrics, target/test counts, coverage,
artifacts, or durable evidence.

## Enable reducers

List every reducer in the server configuration:

```toml
[starlark]
files = [
  "reducers/custom_compiler.star",
  "/opt/bazel-mcp/reducers/company_rules.star",
]
```

Relative paths are resolved from the directory containing the configuration
file. Files are canonicalized, validated, compiled, and frozen when the server
starts. Duplicate names or files, invalid source, API incompatibility, and
invalid selectors are startup errors.

There is no workspace discovery. A `.star` file in a checked-out Bazel
repository is not executed unless an operator adds it to the server
configuration.

## Module contract

Every module exports:

```python
API_VERSION = 1
NAME = "custom-compiler"

def reduce(ctx):
    return None
```

The remaining declarations are optional:

| Name | Default | Meaning |
| --- | --- | --- |
| `PRIORITY` | `0` | Higher priorities run first; names break ties. |
| `MODE` | `"augment"` | `"augment"` or `"override_matching"`. |
| `COMMANDS` | all | Bazel command names such as `build` and `test`. |
| `TARGET_LABELS` | all | Exact labels, `*` wildcards, or prefixes ending in `...`. |
| `TARGET_KINDS` | all | Exact BEP target-kind strings. |
| `ACTION_TYPES` | all | Exact BEP action-type strings. |

Command selection is combined with the event selectors. Multiple event
selector fields are alternatives: a reducer matches when at least one retained
event matches a configured label, target kind, or action type. An
`override_matching` reducer must declare at least one event selector.

The context is a read-only Starlark dictionary with these fields:

| Field | Type | Meaning |
| --- | --- | --- |
| `api_version` | integer | Currently `1`. |
| `command` | string | Canonical Bazel command. |
| `arguments` | list of strings | Bounded startup and command arguments. |
| `exit_code` | integer or `None` | Bazel process exit code. |
| `elapsed_ms` | integer | Bazel wall time. |
| `stdout`, `stderr` | strings | Normalized, bounded, redacted terminal text. |
| `events` | list of dictionaries | Bounded normalized `aborted`, `action`, `target`, and `test_summary` BEP events. |
| `input_truncated` | bool | Whether any custom-reducer input was omitted. |
| `baseline` | dictionary | The bounded, redacted built-in `InvocationSummary`. |

Each event has `ordinal`, `kind`, `label`, `target_kind`, `action_type`,
`success`, `exit_code`, and `message`; fields not provided by that event are
`None`. Event ordinals preserve the original BEP order.

Return `None` when the reducer has nothing to add. Otherwise return a value from
`patch`:

```python
return patch(
    [diagnostic("custom rule failed", category = "compilation")],
    headline = "Custom compiler failed",
)
```

`diagnostic` accepts `message` plus the named arguments `severity`, `category`,
`target`, `action`, `path`, `line`, `column`, and `repetition_count`. Severity is
`error`, `warning`, or `note`. Categories are `workspace`, `loading`,
`analysis`, `visibility`, `action`, `compilation`, `test`, `bazel`, and
`unknown`.

`regex_diagnostics(text, pattern, ...)` runs Rust's linear-time regular
expression engine and returns diagnostics from named capture groups. It
recognizes `message`, `path`, `line`, `column`, `target`, and `action`.
`max_matches` defaults to 50 and cannot exceed 1,000.

See [`examples/reducers/custom_compiler.star`](../examples/reducers/custom_compiler.star)
for a complete reducer.

## Override behavior

`MODE = "augment"` is the default. Its diagnostics are appended to the built-in
result. The first matching reducer in priority order that returns a headline
sets the custom headline.

`MODE = "override_matching"` can set
`suppress_builtin_diagnostics = True` in its patch. Suppression applies only to
built-in diagnostics whose target or action matches the reducer's selected BEP
evidence. Native summary fields and unrelated diagnostics remain intact.

Only the highest-priority matching override reducer owns an invocation. Later
matching overrides are skipped and a bounded collision note is included. This
makes ambiguous ownership visible instead of depending on configuration order.
Augmenting reducers still run.

## Limits and failure behavior

The `[starlark]` table accepts these defaults:

```toml
[starlark]
files = []
max_source_bytes = 262144
max_input_bytes = 1048576
max_events = 10000
max_output_bytes = 65536
max_output_items = 1000
max_ticks = 1000000
max_heap_bytes = 16777216
max_callstack_size = 100
timeout_ms = 100
```

All values must be greater than zero. Inputs are normalized, workspace paths
are replaced, and configured secret patterns are redacted before evaluation.
Returned patches pass through exact schema validation, redaction, diagnostic
deduplication, and the ordinary model-visible byte budget.

The Starlark dialect disables `load` and `print` and exposes no filesystem,
environment, network, process, clock, random, async, or storage APIs. Tick,
heap, call-stack, and wall-clock checks bound accidental runaway evaluation.
This is an in-process extension for operator-approved reducers, not an OS-level
sandbox for hostile code. Run untrusted reducers in a separately isolated
server process.

If a reducer fails at invocation time, the built-in summary is retained and a
bounded note is added. The error is redacted before it reaches diagnostics or
telemetry. If the input projection was truncated, an applied custom result is
marked truncated and points to `bazel.inspect` for local evidence.

## Benchmark

The Criterion benchmark applies the same Rust regular expression and diagnostic
schema through a native reducer and through the Starlark host helper at three
input sizes:

```sh
make bench-reducers
```

It measures reducer application only. Bazel execution, BEP decoding, storage,
and MCP serialization are excluded so the result isolates extension overhead.
The [checked-in performance report](starlark-reducer-performance.md) records the
current native/Starlark comparison.
