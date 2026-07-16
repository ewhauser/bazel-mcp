---
title: Long-running builds
description: Understand synchronous execution, negotiated MCP tasks, polling, cancellation, and restart behavior.
---

`bazel-mcp` can keep ordinary calls synchronous or return durable task handles
when the MCP client declares compatible task support.

## Default: automatic negotiation

```toml
mcp_execution_policy = "auto"
task_ttl_seconds = 86400
task_poll_interval_ms = 2000
```

With `auto`, the server selects the protocol flow from capabilities negotiated
for the current request:

| Client capability | Execution behavior |
| --- | --- |
| No compatible task support | Ordinary synchronous `tools/call` result |
| MCP `2025-11-25` legacy tasks | Nested task handle, then task protocol methods |
| Tasks extension | Extension task result and polling methods |

The task ID is also the Bazel invocation ID, so accepted work is immediately
addressable for status, result retrieval, inspection, and cancellation.

## Policy choices

- `auto` is the recommended default.
- `sync_only` always returns an ordinary `CallToolResult` for new calls.
- `tasks_required` rejects `bazel.run` before Bazel starts when the client did
  not declare compatible task support.

Changing policy does not strand existing unexpired task handles. They remain
readable and cancellable after restart or configuration changes.

## Expiration and recovery

The task TTL is the minimum time a terminal task result remains available. A
queued or running task does not expire. The polling interval is advisory so
clients can avoid excessive status traffic.

For complete settings and constraints, see
[configuration](../../reference/generated/configuration/#configure-negotiated-task-execution).
