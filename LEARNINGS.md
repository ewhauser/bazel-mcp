# Bazel MCP performance issues and bugs

Track concrete performance issues and bugs observed in the MCP server,
especially MCP behavior that makes agents spend extra tokens or make avoidable
tool calls. Each entry must identify the server or protocol symptom, its agent
workflow impact when applicable, and an actionable follow-up. Do not record
general project knowledge, successful behavior, Bazel usage, CI or release
issues, or workflow advice unrelated to MCP efficiency. Do not include secrets
or raw sensitive output.

## Entries

### 2026-07-15

- The initial `bazel.run` evidence did not isolate one failed Rust test, forcing
  a follow-up tool call and a scan of the full job log; prioritize the failing
  test name and assertion so agents avoid fetching unrelated successful-test
  output.

### 2026-07-16

- Listing full durable invocation records exhausted 8 KiB and destructive byte
  shrinking erased IDs and states; keep ledger rows compact and direct users to
  per-invocation views for canonical arguments and detailed diagnostics.
- A 250 ms telemetry debounce coalesced writes efficiently but short-lived stdio
  clients exited first; flush pending counters during graceful service shutdown.
- The `bazel.run` summary retained only `aborting due to 1 previous error`, so
  diagnosing E0308 required another tool call to retrieve the full log; rank the
  primary compiler message and location into the initial failure summary when
  available.
- Encoding and byte-accounting the same MCP result separately duplicated JSON
  work; one-pass encoding improved warm summary inspection by 6.74% and a larger
  query-results view by 8.66% in alternating exact-deliverable comparisons.
- TOON reduced retained MCP result bytes by 35.69% and total provider tokens by
  11.99% versus JSON across 20 verified solves; keep measuring the production
  encoding end to end because payload savings do not map directly to token
  savings.
- Ordinary rules_go compiler messages use `file.go:line:column: message`
  without an `error:` marker; parsing that form into a structured location made
  the exact type error the initial `bazel.run` headline in Bazelisk.
- Collapsing rules_go's multi-line `missing strict dependencies` block into the
  offending source import and a deps hint made the initial response sufficient
  to update the BUILD target without a log inspection.
- A failed Go test can repeat the same assertion in stderr and `test.log`;
  prefer the test-scoped diagnostic, deduplicate the compilation copy, and
  return `inspect_hint=test_log` for optional supporting context.
- Gazelle self-bootstrap failures wrap Go errors in repository-loading output;
  ranking the parsed inner Go location kept the wrapper as supporting evidence
  while making the actionable compiler error the headline.
