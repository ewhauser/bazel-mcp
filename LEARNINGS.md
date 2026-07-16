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

### 2026-07-16

- Listing full durable invocation records exhausted 8 KiB and destructive byte
  shrinking erased IDs and states; keep ledger rows compact and direct users to
  per-invocation views for canonical arguments and detailed diagnostics.
- A 250 ms telemetry debounce coalesced writes efficiently but short-lived stdio
  clients exited first; flush pending counters during graceful service shutdown.
- The Rust compiler summary retained only `aborting due to 1 previous error`, so
  diagnosing E0308 required a log inspection; rank the primary compiler message
  and location into the initial failure summary when available.
- A misspelled BUILD target produced an actionable loading diagnostic including
  Bazel's suggested label in the initial response; preserve that evidence shape.
- Encoding and byte-accounting the same MCP result separately duplicated JSON
  work; one-pass encoding improved warm summary inspection by 6.74% and a larger
  query-results view by 8.66% in alternating exact-deliverable comparisons.
- Matrix workspaces under repository `.cache` were traversed by a later `//...`
  and surfaced unrelated nested-workspace loading failures; create Bazel MCP
  integration scratch roots outside the source workspace by default.
- The Cargo process integration test passed while its Bazel target lacked a
  direct BEP dependency; keep standalone `rust_test` deps aligned with imports
  and include repository-wide Bazel tests in pre-merge validation.
