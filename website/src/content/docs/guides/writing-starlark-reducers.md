---
title: Write a Starlark reducer
description: Build, configure, test, and troubleshoot a target-specific reducer without replacing bazel-mcp's built-in Rust reducers.
---

Use a custom reducer when your Bazel rules emit useful diagnostics that
`bazel-mcp` does not recognize yet. A reducer receives bounded, redacted
invocation evidence and returns a typed headline and diagnostics. The built-in
Rust reducers still run first.

This guide builds an augmenting reducer for a fictional compiler. Keep the
[custom reducer reference](../../reference/generated/custom-reducers/) nearby
for the complete API and limit descriptions.

:::caution
Reducer files are operator-approved code loaded into the server process. Only
configure files you trust. A reducer checked into a Bazel workspace is not
loaded automatically.
:::

## 1. Start with a narrow reducer

Create `reducers/custom_compiler.star` next to your server configuration:

```python
API_VERSION = 1
NAME = "custom-compiler"

COMMANDS = ["build", "test"]
TARGET_LABELS = ["//custom/..."]

def reduce(ctx):
    diagnostics = regex_diagnostics(
        ctx["stderr"],
        r"(?m)^(?P<path>[^:\n]+):(?P<line>[0-9]+):(?P<column>[0-9]+): error: (?P<message>.+)$",
        category = "compilation",
        max_matches = 100,
    )
    if not diagnostics:
        return None
    return patch(
        diagnostics,
        headline = "Custom compiler failed",
    )
```

The required exports are `API_VERSION`, `NAME`, and `reduce(ctx)`. Returning
`None` is an intentional no-op. Returning `patch(...)` adds the typed result to
the built-in summary.

The checked-in
[complete example](https://github.com/ewhauser/bazel-mcp/blob/main/examples/reducers/custom_compiler.star)
also demonstrates priority, target-kind, and action-type selectors.

## 2. Select only relevant invocations

Selectors prevent unrelated builds from paying the evaluation cost or
receiving accidental diagnostics.

| Export | Matches |
| --- | --- |
| `COMMANDS` | Bazel commands such as `build`, `test`, or `coverage`. |
| `TARGET_LABELS` | Exact labels, labels containing `*`, or package prefixes ending in `...`. |
| `TARGET_KINDS` | Exact target-kind strings from normalized BEP target events. |
| `ACTION_TYPES` | Exact action-type strings from normalized BEP action events. |

`COMMANDS` is combined with the event selectors: the command must match and,
when any event selector is present, at least one retained event must match.
Target labels, target kinds, and action types are alternatives to each other.
With no event selector, the reducer applies to every invocation whose command
matches.

Start with the narrowest stable signal. Target-label prefixes are often easier
to maintain than compiler text alone:

```python
COMMANDS = ["build", "test"]
TARGET_LABELS = ["//mobile/custom/..."]
ACTION_TYPES = ["CustomCompile"]
```

## 3. Read the bounded context

The `ctx` dictionary is read-only and contains normalized data:

| Field | Use |
| --- | --- |
| `command`, `arguments` | Understand what Bazel operation was requested. |
| `exit_code`, `elapsed_ms` | Distinguish success, failure, and timing-sensitive messages. |
| `stdout`, `stderr` | Match bounded, normalized, redacted terminal output. |
| `events` | Inspect normalized target, action, test-summary, and aborted BEP events. |
| `baseline` | See the bounded summary produced by the built-in reducers. |
| `input_truncated` | Detect that some reducer input was omitted by a configured limit. |

Each event includes a `kind` plus optional `label`, `target_kind`,
`action_type`, `success`, `exit_code`, and `message` fields. Check optional
fields before using them:

```python
def failed_custom_targets(ctx):
    labels = []
    for event in ctx["events"]:
        if event["kind"] == "target" and event["success"] == False:
            if event["label"] != None:
                labels.append(event["label"])
    return labels
```

Reducers cannot read files, environment variables, the network, processes, or
the clock. Put all required evidence in the invocation output or normalized
BEP events instead.

## 4. Emit typed diagnostics

Use `diagnostic(...)` when the reducer already knows the fields:

```python
diagnostic(
    "generated API is out of date",
    severity = "error",
    category = "action",
    target = "//api:generated",
    action = "ApiCheck",
    path = "api/schema.yaml",
    line = 18,
    column = 3,
)
```

`line` and `column` require `path`. `repetition_count`, when supplied, must be
greater than zero. Use the documented severity and category values so invalid
patches fail visibly instead of producing loosely structured text.

For line-oriented compiler output, `regex_diagnostics` is shorter and uses
Rust's linear-time regular expression engine. Named captures map directly to
diagnostic fields:

- `message`
- `path`, `line`, and `column`
- `target`
- `action`

If `message` is absent, the entire match becomes the message. Bound work with
`max_matches`; its maximum is 1,000.

## 5. Add a headline without hiding native evidence

The default mode is `augment`. It keeps built-in diagnostics and appends yours:

```python
MODE = "augment"

def reduce(ctx):
    item = diagnostic("custom rule failed", category = "action")
    return patch([item], headline = "Custom rule failed")
```

Reducers run by descending `PRIORITY`, then by `NAME`. The first matching
reducer that returns a headline owns the custom headline, so assign priorities
only when multiple reducers can match the same invocation.

## 6. Override only matching evidence

Use override mode when a built-in diagnostic for your selected target or action
would be actively misleading:

```python
API_VERSION = 1
NAME = "legacy-codegen"
PRIORITY = 200
MODE = "override_matching"
COMMANDS = ["build"]
TARGET_LABELS = ["//legacy/codegen/..."]

def reduce(ctx):
    diagnostics = regex_diagnostics(
        ctx["stderr"],
        r"(?m)^CODEGEN ERROR: (?P<message>.+)$",
        category = "action",
    )
    if not diagnostics:
        return None
    return patch(
        diagnostics,
        headline = "Legacy code generation failed",
        suppress_builtin_diagnostics = True,
    )
```

An overriding reducer must declare a target, target-kind, or action-type
selector. Suppression is scoped to built-in diagnostics associated with the
matched target or action; unrelated evidence remains. Only the highest-priority
matching override owns an invocation, and collisions produce a bounded notice.

Prefer augment mode unless duplicate native evidence is demonstrably harmful.

## 7. Enable and reload the reducer

Add the file explicitly to the server configuration:

```toml
[starlark]
files = ["reducers/custom_compiler.star"]
```

Relative paths are resolved from the configuration file, not the Bazel
workspace or current directory. Restart the MCP server after changing the file.
Reducers are parsed, validated, compiled, and frozen at startup; syntax errors,
duplicate names, invalid exports, and invalid selectors prevent startup.

The [configuration reference](../../reference/generated/configuration/)
documents input, output, event, heap, instruction, stack, and timeout limits.

## 8. Exercise a real failure

Use a small target that reliably emits one representative failure:

1. Restart the server and confirm it starts without a reducer load error.
2. Run the focused target through `bazel.run`.
3. Confirm the custom headline and structured location appear once.
4. Run an unrelated target and confirm the reducer is a no-op.
5. Add a repeated error case and confirm `max_matches` keeps the result bounded.
6. If using override mode, confirm unrelated built-in diagnostics remain.

Keep a small output fixture beside organization-managed reducers. When the
compiler format changes, update the fixture and review the diagnostic diff
before deploying the reducer.

## Troubleshoot a reducer

| Symptom | Check |
| --- | --- |
| The server does not start | Required exports, unique `NAME`, valid mode, and selector syntax. |
| The reducer never runs | `COMMANDS`, retained BEP event fields, and overly narrow selectors. |
| The reducer runs but returns nothing | Whether the message is in `stdout` or `stderr`, multiline regex flags, and named captures. |
| A patch is rejected | Severity/category spelling, `path` for locations, positive counts, and output limits. |
| Native diagnostics remain | The mode, `suppress_builtin_diagnostics`, and whether native target/action fields match selected evidence. |
| Results are marked truncated | `ctx["input_truncated"]`, `max_input_bytes`, `max_events`, and the ordinary response budget. |

Runtime failures never discard the built-in result. The server logs a redacted
error and adds a bounded note, which makes reducer failures visible without
turning a useful Bazel summary into an empty response.

## Production checklist

- Give every reducer a stable, unique name.
- Select the smallest command and target/action scope.
- Return `None` when no actionable diagnostic exists.
- Prefer typed helpers over hand-built dictionaries.
- Cap regex matches and avoid processing the same text repeatedly.
- Keep common, high-volume behavior in native reducers.
- Treat reducer changes like code changes and review representative outputs.

See the [performance comparison](../../reference/generated/starlark-reducer-performance/)
before applying many Starlark reducers to the same invocation.
