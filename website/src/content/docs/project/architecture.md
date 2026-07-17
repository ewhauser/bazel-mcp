---
title: Architecture
description: A guided map of the bazel-mcp runtime, crate boundaries, evidence flow, and design specifications.
---

The architecture keeps MCP transport, Bazel execution, policy, evidence
storage, and deterministic reduction separate.

## Runtime flow

```text
MCP client
   │ stdio JSON-RPC
   ▼
bazel-mcp-server ── policy ── runner ── Bazel
                                  │
                 ┌────────────────┴──────────────┐
                 ▼                               ▼
          Build Event Protocol              stdout/stderr
                 │                               │
                 └────────► local store ◄────────┘
                                  │
                                  ▼
                       deterministic reducers
                                  │
                     bounded run/inspect results
```

Only the server crate depends on the MCP implementation library. Domain types,
policy, process management, storage, Build Event Protocol ingestion, and
reducers remain usable without MCP transport concerns.

Text diagnostic reduction has its own source-agnostic leaf crate. It accepts
ordinary command-output byte slices, applies deterministic compiler and test
parsers, redaction, exact deduplication, ranking, and serialized byte budgets.
The Bazel reducer remains the adapter that combines those diagnostics with BEP
targets, actions, artifacts, test metadata, and Bazel-specific headlines.

Production invocation storage is database-free filesystem storage. Raw
evidence stays local while model-visible results, durable metadata, and
telemetry pass through redaction and fixed byte budgets.

## Design specifications

The repository keeps detailed decisions as reviewable specifications:

- [001: Product requirements](https://github.com/ewhauser/bazel-mcp/blob/main/specs/001-product-requirements.md)
- [002: Project architecture](https://github.com/ewhauser/bazel-mcp/blob/main/specs/002-project-architecture.md)
- [003: Negotiated MCP task execution](https://github.com/ewhauser/bazel-mcp/blob/main/specs/003-configurable-mcp-task-execution.md)
- [004: Outcome-aware evidence retention](https://github.com/ewhauser/bazel-mcp/blob/main/specs/004-outcome-aware-evidence-retention.md)

These are implementation records rather than a first-stop user guide. Start
with [how it works](../../concepts/how-it-works/) for the product model.
