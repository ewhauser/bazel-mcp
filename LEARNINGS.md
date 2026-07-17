# Bazel MCP performance issues and bugs

Track concrete performance issues and bugs observed in the MCP server,
especially MCP behavior that makes agents spend extra tokens or make avoidable
tool calls. Each entry must identify the server or protocol symptom, its agent
workflow impact when applicable, an actionable follow-up, and the Codex Thread
ID where the observation occurred. Do not record general project knowledge,
successful behavior, Bazel usage, CI or release issues, or workflow advice
unrelated to MCP efficiency. Do not include secrets or raw sensitive output.

## Entries

### 2026-07-15

- The initial `bazel.run` evidence did not isolate one failed Rust test, forcing
  a follow-up tool call and a scan of the full job log; prioritize the failing
  test name and assertion so agents avoid fetching unrelated successful-test
  output.

### 2026-07-16

- Separate bazel-mcp processes sharing an output base bypassed the in-memory
  scheduler, while direct Bazel contention remained an opaque portion of wall
  time; coordinate every known output-base request with a user-scoped advisory
  lock, preserve Bazel's native wait-and-takeover behavior, and expose only a
  bounded owner label plus the combined wait duration. Thread:
  `019f6db5-c3bf-77c1-9524-4fed404237c0`.
- A live Bazel 9.2 contention run waited behind an uncoordinated client and
  successfully restarted for the next workspace, but emitted no lock-wait text
  before takeover; the stderr observer therefore sent no wait phase and
  recorded zero wait milliseconds, leaving the delay opaque to the agent.
  Observe sanitized owner changes in the known explicit output-base lock file
  instead of relying only on Bazel stderr markers. Thread:
  `019f6db5-c3bf-77c1-9524-4fed404237c0`.
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
- Keep custom-reducer BEP collection opt-in and project only normalized fields;
  the no-reducer path can continue dropping raw frames without extension
  allocation or model-visible evidence growth.
- The Starlark diagnostic adapter measured 25.06x to 134.74x native reducer
  latency, but stayed below 10.75 ms through 1,000 matches; retain Rust for
  common reducers and use Starlark for explicitly configured rule-specific
  logic where this post-Bazel latency is acceptable.
- Starlark's in-process tick, heap, stack, and timeout limits isolate accidental
  runaway reducers, not hostile code; keep workspace auto-discovery disabled
  and require a separate OS-isolated server before accepting untrusted scripts.
- `bazel.run mod deps` refreshed the rules_rs crate extension lock with a compact
  success summary and no raw dependency output; use that informational command
  plus a local lockfile diff for token-efficient Cargo dependency changes.
- Python precompilation surfaced `Unhandled error:` ahead of a retained
  `SyntaxError` and omitted the source line; pair standard traceback frames with
  terminal exception classes so agents can edit the failing file without a log
  inspection. Thread: `019f6b89-e945-78a0-9264-a6ad416905a1`.
- Python import and assertion failures retained the terminal exception but
  discarded the preceding runfiles source frame, while a trailing `FAILED`
  summary could replace richer test evidence; prefer located traceback causes
  and keep `test_log` as optional context. Thread:
  `019f6b89-e945-78a0-9264-a6ad416905a1`.
- BUILD and `.bzl` syntax errors retained the parser message but lost its inline
  location and ranked package-loading wrappers first; parse Starlark source
  coordinates before diagnostic ranking. Thread:
  `019f6b89-e945-78a0-9264-a6ad416905a1`.
- Starlark macro and rule failures split the innermost `File` frame from the
  terminal `Error in fail`, forcing log inspection or returning a large BEP
  wrapper; pair them into a concise loading or analysis diagnostic. Thread:
  `019f6b89-e945-78a0-9264-a6ad416905a1`.
- Javac diagnostics retained the source path inside the message but discarded
  its structured location and the following `symbol:` detail; pair the bounded
  compiler block before ranking. Thread:
  `019f6b89-e945-78a0-9264-a6ad416905a1`.
- Java test exceptions were recognized as Python-style compilation failures,
  while their application stack frames remained only in `test_log`; prefer the
  first non-framework JVM frame as located test evidence. Thread:
  `019f6b89-e945-78a0-9264-a6ad416905a1`.
- Protoc source diagnostics omit generic `error:` markers, so syntax, import,
  and schema failures were discarded in favor of Bazel action wrappers; parse
  the `.proto:line:column` form directly. Thread:
  `019f6b89-e945-78a0-9264-a6ad416905a1`.
- Missing protobuf imports emit an unlocated missing-file line before the
  editable import declaration and dependent type errors; rank the located
  declaration first and preserve the follow-on diagnostics. Thread:
  `019f6b89-e945-78a0-9264-a6ad416905a1`.
- TypeScript compiler diagnostics use `file(line,column): error TS...` without
  the generic marker the reducer expected, leaving Bazel's completion wrapper
  as the headline; parse the tsc code and location directly. Thread:
  `019f6b89-e945-78a0-9264-a6ad416905a1`.
- Node exceptions were retained as unlocated compilation evidence while their
  source headers and application stack frames stayed only in `test.log`; pair
  bounded Node frames and prefer the test-scoped diagnostic. Thread:
  `019f6b89-e945-78a0-9264-a6ad416905a1`.
- Clang C++ diagnostics were already selected as headlines, but their source
  coordinates remained embedded in the message; structure common Clang/GCC
  and MSVC forms so agents can edit the exact location directly. Thread:
  `019f6b89-e945-78a0-9264-a6ad416905a1`.
- C++ link failures promoted `clang: linker command failed` while discarding
  Apple ld's preceding undefined symbol; pair bounded platform-specific linker
  evidence before ranking wrappers. Thread:
  `019f6b89-e945-78a0-9264-a6ad416905a1`.
- Gtest assertion details and C++ exception descriptions remained only in
  `test.log`, leaving the initial response at the failed target or `unknown
  file: Failure`; reduce the bounded gtest failure block into one test-scoped
  diagnostic. Thread: `019f6b89-e945-78a0-9264-a6ad416905a1`.
- JavaScript test-log promotion retained the same Node exception twice, once
  with its application location and once without it; deduplicate equivalent
  test-scoped diagnostics in favor of the located form to preserve item and
  byte budget. Thread: `019f6b89-e945-78a0-9264-a6ad416905a1`.
- Starlark and Bazel analysis recordings produced an empty analysis diagnostic
  beside the actionable failure, consuming model-visible item budget; discard
  empty reducer messages before ranking and serialization. Thread:
  `019f6b89-e945-78a0-9264-a6ad416905a1`.
- A successful 1,000-row `bazel.run query` still normalized, deduplicated,
  redacted, and serialized the bounded capture as failure-evidence log data;
  DHAT measured about 736 KB in 5,222 allocations per call across ten
  end-to-end MCP requests even though agents consume `query_results`. Defer log
  materialization until reduction selects the `log` view, and skip it for
  ordinary successful queries. Thread:
  `019f6db7-4010-7191-8796-1c83ff2a7f42`.
- A filtered `bazel.inspect query_results` scan over 100,000 rows allocated a
  fresh line, transformed value, lowercase match string, and serialized clone
  per row: 14.88 MB across 401,020 allocations. Reuse decoding,
  transformation, and serialization buffers, compare ASCII case in place, and
  transfer ownership only for selected rows. Thread:
  `019f6db7-4010-7191-8796-1c83ff2a7f42`.
- A 1,000-match custom Starlark reduction converted the complete reducer
  context and every diagnostic through JSON, then materialized and reparsed
  the output JSON; DHAT measured 15.45 MB in 234,958 allocations for one
  application. Build typed Starlark dict/list values directly, count bounded
  output bytes without materializing JSON, decode the typed patch in place,
  and extend the nested-schema contract test when reducer context fields
  evolve. Thread: `019f6db7-4010-7191-8796-1c83ff2a7f42`.
- Recorded BEP strings used fixed-length uppercase workspace markers while
  service locations used lowercase markers, complicating live/replay parity and
  leaking padding into visible messages; normalize both marker forms before
  projecting diagnostics and artifact URIs. Thread:
  `019f6b89-e945-78a0-9264-a6ad416905a1`.
