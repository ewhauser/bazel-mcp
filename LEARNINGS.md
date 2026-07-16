# Bazel MCP learnings

Track concrete opportunities discovered while using Bazel MCP in this
repository. Add concise, actionable entries under the current date, including
the observed workflow or result and the improvement it suggests. Focus on MCP
server behavior, result reduction, inspection patterns, and model-visible token
efficiency. Do not include secrets or raw sensitive output.

## Entries

### 2026-07-15

- CI diagnosis required scanning a full job log to isolate one failed Rust test;
  prioritize the failing test name and assertion in initial Bazel MCP evidence so
  agents can avoid fetching unrelated successful-test output.
