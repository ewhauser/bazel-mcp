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
When work involves Bazel, create or update the repository-root `LEARNINGS.md`
with concrete potential improvements and opportunities to use the Bazel MCP
server more token-efficiently. Keep entries concise, actionable, and grounded
in observations from the current work; do not record secrets or raw sensitive
output.
