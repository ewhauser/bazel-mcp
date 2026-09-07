# Bazel MCP performance issues and bugs

This file is a priority-ordered queue of unresolved MCP server or protocol
problems. Keep observations that expose avoidable tool calls, excess
model-visible tokens, or measurable server overhead. Remove an entry after its
fix is verified; implementation history and benchmark results belong in commits,
issues, and reports.

Each entry must identify the symptom, workflow impact, actionable follow-up,
and Codex Thread ID. Do not record successful behavior, general project
knowledge, Bazel usage, CI or release issues, or workflow advice unrelated to
MCP efficiency. Do not include secrets or raw sensitive output.

## Highest priority remaining

### Empty analysis diagnostics obscure the failure headline

- Symptom: `bazel.run` for a toolchain-resolution failure returned `headline: "Bazel failed: "` and two diagnostics with empty messages, although another diagnostic contained the actionable error. Invocation: `01a07dec-fd69-77f0-aed2-02f829804b94`.
- Impact: the headline did not identify the cause; the response spent diagnostic entries and model-visible tokens on empty messages, requiring a scan of the remaining diagnostics.
- Follow-up: discard empty normalized diagnostics before deduplication and select the first nonempty actionable message for the headline. Add a regression fixture for a toolchain-analysis failure with empty BEP failure messages.
- Codex Thread ID: `01a07230-796a-77d1-9609-55bc1b9e4b4c`.
