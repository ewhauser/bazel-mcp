---
title: Tools
description: "The complete public bazel-mcp tool surface: bazel.run, bazel.inspect, and bazel.cancel."
---

The server intentionally exposes exactly three tools. Task status and results,
when negotiated, use MCP protocol methods rather than additional tools.

## `bazel.run`

Run an allowed Bazel command, or an operator-configured Aspect CLI command, and
receive a bounded result.

```json
{
  "workspace": "/src/project",
  "command": "test",
  "args": ["//services/...", "--test_tag_filters=-integration"]
}
```

Supported command classes include `build`, `test`, `coverage`, `query`,
`cquery`, `aquery`, and selected informational commands. The exact policy is
configurable. Operators can opt specific commands such as `lint` into Aspect
CLI routing without adding another MCP tool. Request arguments remain an array
and are never concatenated into a shell command.

The result includes the invocation lifecycle state, command outcome, a bounded
headline and diagnostics, and an `invocation_id` when evidence can be inspected.

## `bazel.inspect`

Read a narrow view from a retained invocation. Available views include:

- summary and diagnostics;
- test results and copied failed-test logs;
- coverage and artifacts;
- query results;
- normalized logs.

```json
{
  "invocation_id": "019f...",
  "view": "test_log",
  "limit": 20
}
```

Views are filtered, byte-bounded, and paginated. When a response is truncated,
pass its opaque `next_cursor` to the next inspection request without modifying
it.

## `bazel.cancel`

Cancel a queued or running invocation.

```json
{
  "invocation_id": "019f..."
}
```

Cancellation uses the same durable invocation ID as execution and inspection.
The server coordinates interrupt and termination grace periods rather than
leaving process cleanup to the agent.

## Long-running calls

When the client and server negotiate compatible MCP task support, `bazel.run`
can return a durable task handle. Task polling and result retrieval are MCP
protocol methods; the three-tool inventory does not change.

See [long-running builds](../guides/long-running-builds/) for execution modes.
