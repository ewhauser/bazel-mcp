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
- Release Please created the Bazel-strategy tag with `GITHUB_TOKEN`, so the
  tag-push cargo-dist workflow never ran; call artifact publication directly
  from the release output and retain a manual tag backfill path.
- Add result encoding as an explicit agentic adapter dimension so JSON and TOON runs share identical MCP tools, prompts, task snapshots, and verifier controls.
- TOON reduced retained MCP result bytes by 35.69% and total provider tokens by 11.99% with 20/20 verified solves; keep encoding comparisons end-to-end because payload savings do not translate directly to provider-token savings.
- Report command-output outliers separately from MCP result bytes; one unbounded source search added 472,157 bytes and materially inflated the active-token comparison despite leaving the total-token direction unchanged.
- Keep production-default benchmark targets aligned with the server serialization default; retain explicit encoding adapters for controlled format comparisons.
- Pin representation-sensitive protocol harnesses to an explicit result
  encoding so changing the production default does not invalidate their parser.
- Supported Bazel majors are repeated across runtime defaults, matrix scripts,
  golden generation, and docs; define one canonical source so compatibility
  updates require less repository-wide discovery and cannot drift silently.
- The matrix consumed smoke output through process substitution and hid producer
  failures; complete the MCP client first so encoding regressions fail CI.
- A real BES-backed `bazel.run build //:bazel-mcp` retained hundreds of events,
  but transport counts were available only in stderr tracing. Add transport,
  event-count, and retained-byte metrics to an opt-in diagnostic view so future
  investigations avoid reading raw evidence.
- Warm `bazel.run` responses expose millisecond invocation duration, which was
  enough to show tail/BES parity but not isolated capture costs. Keep the
  repeatable BEP transport benchmark for microsecond-resolution regressions.
- Exercise both `tail` and `bes` across the Bazel version matrix; the same
  reducer fixtures can validate identical summaries without model-visible raw
  BEP output.
