# Agent instructions

This repository implements a token-efficient, local Bazel MCP invocation
service. Preserve these invariants:

- The server exposes exactly `bazel.run`, `bazel.inspect`, and `bazel.cancel`.
- MCP stdout is protocol-only; tracing and diagnostics go to stderr.
- Never invoke Bazel through a shell or concatenate request arguments.
- Preserve raw evidence locally while enforcing model-visible byte budgets.
- Keep the crate dependency direction in specification 002. Only the server
  depends on `rmcp`; the production store is database-free filesystem storage.
- Keep reducers deterministic. Fixture updates require reviewed golden diffs.
- Redact secrets before summaries, durable metadata, and telemetry.

Use `make build`, `make test`, `make check`, `make test-bazel-matrix`,
`make fuzz-smoke`, and the explicit token benchmark targets. Do not run the long
Abseil benchmark as an ordinary unit test. Use Conventional Commits; Release
Please owns versions and the changelog.

Always use the Bazel MCP tools instead of invoking Bazel or Bazelisk directly.
When using Bazel MCP reveals a performance issue, bug, or agent-workflow
inefficiency caused by MCP behavior, create or update the repository-root
`LEARNINGS.md`. Record the concrete MCP symptom, its impact on tool calls or
model-visible tokens when applicable, an actionable follow-up, and the Codex
Thread ID where the observation occurred.
Agent-workflow observations are especially valuable when they expose
inefficient protocol behavior, result reduction, or inspection flows. Do not
add general project, Bazel, CI, release, or unrelated workflow learnings. Do not
record secrets or raw sensitive output.
