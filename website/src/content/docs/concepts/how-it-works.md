---
title: How it works
description: Follow a Bazel invocation from an MCP request to bounded results and retained local evidence.
---

`bazel-mcp` separates **what the model needs now** from **what must remain
available as evidence**.

## The evidence lifecycle

1. **The client submits structured arguments.** The server accepts a workspace,
   an allowed command and an argument array. It never constructs a shell
   command.
2. **Bazel or a configured Aspect command runs locally.** Existing workspace
   credentials, toolchains, remote execution settings, and caches continue to
   apply.
3. **Evidence is captured privately.** Output and Build Event Protocol data are
   retained under the local invocation cache.
4. **Deterministic reducers find the useful result.** Concrete root causes,
   failed tests, coverage, artifacts, and query results are normalized,
   deduplicated, redacted, and packed into a fixed byte budget.
5. **The agent inspects only when needed.** `bazel.inspect` exposes filtered,
   paginated views using the invocation ID returned by `bazel.run`.

```text
agent request
     │
     ▼
policy ──► local Bazel/Aspect process ──► private evidence
                                      │
                                      ▼
                               deterministic reducer
                                      │
                         bounded result + invocation ID
                                      │
                        bazel.inspect for narrow follow-up
```

## Response budgets

Successful `bazel.run` results are limited to 2 KiB and unsuccessful results to
8 KiB. Inspection pages are also bounded. Requested item counts are maxima;
byte packing can return fewer items plus an opaque cursor for the next page.

The budget applies to the model-visible representation. The complete retained
evidence remains local until retention policy removes it.

## Result encodings

The logical result can be represented as TOON, compact JSON text, MCP
`structuredContent`, or both. TOON is the default because it keeps the same
evidence while reducing the serialized payload.

See [configuration](../../reference/generated/configuration/#choose-a-result-encoding)
for host compatibility choices.

## Security boundary

The server is not a remote build service. It is a local execution boundary:

- commands are allowlisted;
- arguments are passed directly to Bazel without a shell;
- sensitive text is redacted before summaries, durable metadata, and telemetry;
- raw evidence remains local and should still be treated as sensitive;
- allowed workspace roots can restrict where requests run.

Read the complete [security policy](../../reference/generated/security/) before
using the server in a shared environment.
