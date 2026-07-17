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

1. **Make shared-store startup failures actionable.** Starting a second stdio
   server against the default cache root failed before MCP initialization with
   only `open invocation store`, forcing a retry with isolated cache and output
   roots plus a cold Bazel build. Surface the store path and underlying error;
   safely recover from, or clearly identify, an incompatible or corrupt shared
   root. Thread: `019f6e76-0ae9-7622-97ec-49f233a3ee8c`.

2. **Preserve Rust compiler root causes in the initial result.** A failed
   `bazel.run` retained only `aborting due to 1 previous error`, so diagnosing
   E0308 required a log inspection. Parse Rust error codes and their
   `--> file:line:column` locations, then rank that diagnostic ahead of rustc's
   terminal summary. Thread: `019f6990-d0eb-7162-9e6b-98945fe70530`.

3. **Return bounded context for filtered test logs.** Filtering
   `bazel.inspect test_log` to a failing test name retained the panic header but
   dropped the adjacent `Result` error because the continuation line did not
   repeat the filter text. Return a small context window around matches so the
   causal message remains visible without a targeted rerun. Thread:
   `019f6df4-be14-75b2-8e2b-654b60a669c3`.
