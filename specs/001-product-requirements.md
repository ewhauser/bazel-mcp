# Bazel MCP Invocation Service

| Field | Value |
| --- | --- |
| Status | Draft |
| Specification | 001 |
| Product | `bazel-mcp` |
| Last updated | 2026-07-14 |
| Primary implementation language | Rust |

## 1. Summary

`bazel-mcp` is a local-first Model Context Protocol (MCP) server that owns the
complete lifecycle of Bazel invocations on behalf of coding agents. It executes
Bazel directly, captures stdout and stderr without placing them in the model
context, ingests the Build Event Protocol (BEP), stores a durable result keyed by
an invocation UUID, and returns a small, structured summary. Agents can request
bounded follow-up details for diagnostics, tests, coverage, artifacts, query
results, or logs.

The primary product outcome is lower LLM token usage. The service MUST reduce
both:

1. Repeated model turns and polling calls while Bazel is running.
2. Model-visible Bazel output, especially progress rendering, repeated warnings,
   compiler output, and test logs.

The product is a general Bazel command executor with command-specific result
reducers. It is not a raw shell proxy, and it is not only a diagnostics database.

## 2. Motivation

Experiments shared in the source discussion showed that better orchestration of
long-running Bazel commands reduced model events from 13 to 5, polling calls from
10 to 3, total input tokens by 57.6%, and model-visible tool-output bytes by
45.2%, with approximately 1.4% additional Bazel wall time. A separate local MCP
implementation backed by streaming BEP was reported to reduce Bazel-related
token usage by approximately 85%, although that result did not include a
reproducible benchmark methodology.

These observations suggest two independent sources of waste:

- Every polling turn can cause the model to process a large cached context again,
  so reducing model events matters even when individual tool outputs are small.
- Bazel terminal output is optimized for an interactive human terminal, not for
  a language model. It often includes progress updates, duplicate warnings, and
  logs that are irrelevant to the next agent action.

An MCP server that only reads the "last" externally ingested BEP snapshot does
not fully solve the problem. Execution still depends on agent instructions,
results are ambiguous under concurrency, and the agent may still stream Bazel
output through a shell tool. By owning execution, `bazel-mcp` can bind the
command, process, BEP events, logs, artifacts, and follow-up reads to one durable
invocation.

## 3. Product principles

1. **One invocation, one identity.** Every command and every derived result is
   associated with an immutable UUID. The product MUST NOT expose a global "last
   build" concept.
2. **Bounded by default.** No default tool response may contain unbounded command
   output. More detail is explicitly requested and paginated.
3. **Preserve evidence.** Raw output is captured before reduction and retained as
   a fallback. Token efficiency MUST NOT depend on discarding the only useful
   diagnostic source.
4. **One model-visible call when possible.** A normal invocation waits for
   completion or uses MCP task-augmented execution. The agent does not poll a
   terminal process.
5. **Preserve Bazel semantics.** The server may change terminal presentation and
   add observability outputs, but MUST NOT silently change build outputs,
   configurations, caching, remote execution, or target meaning.
6. **General executor, specialized reducers.** The process runner accepts Bazel
   commands generically. Reducers understand the different result shapes of
   build, test, coverage, query, and informational commands.
7. **Local-first and least privilege.** Stdio is the default transport. Workspace
   roots and sensitive commands are policy-controlled.

## 4. Goals

### 4.1 Primary goals

- Reduce total model input and output tokens attributable to Bazel workflows by
  at least 75% on the acceptance benchmark.
- Complete an ordinary build or test in one model-visible tool call on hosts that
  support a sufficiently long tool request or MCP task-augmented execution.
- Return actionable root-cause information without sending full Bazel output to
  the model.
- Execute common Bazel commands through one consistent MCP interface.
- Maintain durable, concurrency-safe results addressable by invocation UUID.
- Preserve Bazel daemon reuse and the repository's existing `.bazelrc` behavior.

### 4.2 Secondary goals

- Support multiple workspaces and worktrees without result confusion.
- Make token savings, returned bytes, raw bytes, and wall-time overhead
  measurable.
- Provide a foundation for future BuildBuddy, BES, remote CAS, and
  language-specific diagnostic integrations.

## 5. Non-goals

The first production release will not:

- Replace BuildBuddy or another organization-wide Build Event Service.
- Provide a general-purpose shell execution tool.
- Guarantee a universal structured diagnostic parser for every compiler or
  custom Bazel action.
- Retrieve arbitrary `bytestream://` or remote CAS artifacts without an explicit
  configured integration.
- Create a new Bazel output base for each invocation.
- Run interactive programs or tests that require a terminal.
- Implement a distributed scheduler across machines.
- Use TOON or another non-standard serialization as a prerequisite for token
  savings.
- Support Windows in the MVP. Windows process cancellation and job-object support
  are a later compatibility milestone.

## 6. Users and primary workflows

### 6.1 Primary user

The primary user is an autonomous or interactive coding agent working in a Bazel
workspace. The human user supervises the agent and retains control over command
execution through the MCP host's normal tool approval model.

### 6.2 Primary workflows

#### Build or test succeeds

1. The agent calls `bazel.run` once.
2. The server queues and executes the command, capturing output privately.
3. The server returns a concise success summary including duration, target/test
   counts, and invocation ID.
4. No follow-up call is required.

#### Build or test fails

1. The agent calls `bazel.run` once.
2. The server reduces BEP failures and relevant stderr into a bounded list of root
   causes.
3. The response identifies failed targets/tests and includes the most actionable
   diagnostics.
4. If the response reports additional details, the agent calls `bazel.inspect`
   with a narrow view and filter.

#### Large query

1. The agent calls `bazel.run` with `query`, `cquery`, or `aquery`.
2. The server selects a machine-readable output adapter, streams results into
   storage, and returns a count plus a small sample.
3. The agent pages or filters results using `bazel.inspect` rather than receiving
   the entire graph.

#### Cancellation

1. The MCP host cancels the request or the agent calls `bazel.cancel`.
2. The server requests graceful Bazel cancellation and escalates if needed.
3. The invocation is retained with state `cancelled` and any available partial
   diagnostics.

## 7. Terminology

- **Invocation:** One execution of a Bazel client command, identified by UUID.
- **Build-like command:** A command such as `build`, `test`, or `coverage` for
  which BEP is the primary structured result source.
- **Reducer:** Code that converts BEP events or captured text into a bounded,
  agent-oriented result.
- **Raw output:** The complete captured stdout and stderr byte streams.
- **Model-visible output:** Tool result content that the MCP host may place in an
  LLM context.
- **Workspace:** A canonical directory containing `MODULE.bazel`,
  `WORKSPACE.bazel`, or `WORKSPACE`.
- **Internal flag:** A Bazel flag owned by `bazel-mcp` for identity, capture, or
  terminal suppression and not overridable by a tool caller.

## 8. MCP interface

The server MUST expose exactly three tools in the MVP. It MUST NOT create
separate `fetch_*`, `find_*`, or per-command tools for equivalent data.

### 8.1 `bazel.run`

#### Purpose

Execute one Bazel invocation and return its bounded result.

#### Input

```json
{
  "workspace": "/absolute/path/to/workspace",
  "startup_args": [],
  "command": "test",
  "args": ["//foo/...", "--config=ci"],
  "timeout_seconds": 1800
}
```

| Field | Required | Type | Requirements |
| --- | --- | --- | --- |
| `workspace` | Yes | Absolute path | Must resolve within an allowed workspace root. |
| `startup_args` | No | Array of strings | Defaults to `[]`; passed before the Bazel command. |
| `command` | Yes | String | Validated against the configured command policy. |
| `args` | No | Array of strings | Defaults to `[]`; passed after the Bazel command. |
| `timeout_seconds` | No | Integer | Uses the server default when omitted; bounded by server policy. |

The server MUST pass arguments directly to a child process. It MUST NOT join or
evaluate them as shell text.

#### Execution behavior

- The tool waits for completion by default.
- The tool declares MCP task support as optional when supported by the selected
  `rmcp` release.
- A task-capable host may receive a task handle without changing the tool schema.
- A non-task host uses one long-running tool request.
- Manual client polling is not part of the normal path.
- A Bazel exit code other than zero is a successful MCP tool execution whose
  result has `state: "failed"`.
- MCP `isError` is reserved for failures to validate or execute the tool itself,
  such as an invalid workspace or failure to spawn the Bazel client.

#### Output

The logical result contains:

```json
{
  "invocation_id": "019...",
  "state": "failed",
  "command": "test",
  "exit_code": 3,
  "duration_ms": 42108,
  "targets": {
    "requested": 18,
    "succeeded": 16,
    "failed": 2
  },
  "tests": {
    "passed": 72,
    "failed": 1,
    "flaky": 0,
    "skipped": 2
  },
  "diagnostics": [
    {
      "target": "//foo:foo_test",
      "kind": "test_failure",
      "summary": "expected 3, received 4",
      "source": "bazel://invocations/019.../tests/foo_test"
    }
  ],
  "available_views": ["diagnostics", "tests", "artifacts", "log"],
  "more_available": true
}
```

Fields that are not applicable MAY be omitted. `invocation_id`, `state`,
`command`, and `duration_ms` are always present. `exit_code` is null only when no
exit code exists, such as certain spawn failures or forced termination.

### 8.2 `bazel.inspect`

#### Purpose

Read a filtered, paginated view of one invocation.

#### Input

```json
{
  "invocation_id": "019...",
  "view": "diagnostics",
  "filter": "//foo/...",
  "limit": 20,
  "cursor": null
}
```

| Field | Required | Type | Requirements |
| --- | --- | --- | --- |
| `invocation_id` | Yes | UUID string | Must identify a retained invocation. |
| `view` | Yes | Enum | One of `summary`, `diagnostics`, `tests`, `coverage`, `artifacts`, `query_results`, or `log`. |
| `filter` | No | String | Label glob or literal substring, depending on view; not an arbitrary regular expression. |
| `limit` | No | Integer | Defaults to 20; range 1 through 100. |
| `cursor` | No | Opaque string | Continues a prior view using stable server-side ordering. |

#### Output

Every inspection response contains:

- `invocation_id`
- `view`
- `items`
- total and filtered counts where known
- `next_cursor` when additional items exist
- `truncated: true` if a byte cap, rather than the item limit, stopped output

View requirements:

- `summary` returns the canonical invocation summary.
- `diagnostics` returns reduced analysis, loading, action, and compiler failures.
- `tests` returns target, run, shard, attempt, status, duration, cache status, and
  bounded failure details.
- `coverage` returns per-file LCOV summaries when a local coverage artifact is
  available and otherwise returns artifact references plus an explicit
  availability reason.
- `artifacts` resolves BEP `NamedSetOfFiles` references and returns bounded file
  metadata without reading artifact contents.
- `query_results` returns stored query rows using stable pagination.
- `log` returns a bounded logical page of captured stdout or stderr. It MUST NOT
  return an entire unbounded file. The default is the combined failure-oriented
  tail.

### 8.3 `bazel.cancel`

#### Purpose

Cancel a queued or running invocation.

#### Input

```json
{
  "invocation_id": "019...",
  "reason": "No longer needed"
}
```

#### Output and behavior

- Cancellation is idempotent.
- Cancelling a queued invocation transitions it directly to `cancelled`.
- Cancelling a running invocation starts the graceful cancellation sequence.
- Cancelling a completed invocation returns its current terminal state and
  `cancellation_requested: false`.
- The result includes the invocation ID, prior state, current state, and whether
  a cancellation signal was sent.

### 8.4 MCP resources

Tools MAY return `bazel://` resource links for stored details. Resource links are
references, not a mechanism for bypassing response limits. Any `read_resource`
implementation MUST enforce the same filtering, pagination, permissions, and
byte ceilings as `bazel.inspect`.

Resources MUST NOT expose arbitrary local filesystem paths outside an allowed
workspace or the invocation store.

### 8.5 Result encoding

The server supports four deployment-level result encodings:

- `text`: one compact JSON `TextContent` block and no duplicate
  `structuredContent`; this is the default.
- `toon`: one TOON-encoded `TextContent` block and no duplicate
  `structuredContent`.
- `structured`: structured content for hosts proven to handle it without placing
  a duplicate representation in model context.
- `both`: structured content plus the backwards-compatible text representation.

The encoding is server configuration, not a per-call argument. The benchmark
MUST use the encoding intended for the production MCP host.

## 9. Workspace and Bazel discovery

### 9.1 Workspace validation

- `workspace` MUST be absolute and canonicalized. When allowed roots are
  configured, it MUST be contained by one of them.
- The canonical directory MUST contain `MODULE.bazel`, `WORKSPACE.bazel`, or
  `WORKSPACE`.
- Symlink traversal MUST NOT escape the allowed root.
- Worktrees are distinct workspaces even when they share Git object storage.

### 9.2 Bazel executable resolution

For each workspace, the server resolves the executable in this order:

1. An explicitly configured per-workspace executable or wrapper.
2. `<workspace>/tools/bazel` when present and executable.
3. `bazelisk` from the server's configured executable search path.
4. `bazel` from the server's configured executable search path.

The caller cannot supply an arbitrary executable path through `bazel.run`.
Resolution is recorded in invocation metadata.

## 10. Command policy and adapters

### 10.1 Default command classes

| Class | Commands | Default policy | Reducer |
| --- | --- | --- | --- |
| Build-like | `build`, `test`, `coverage` | Allowed | BEP plus bounded text fallback |
| Graph query | `query`, `cquery`, `aquery` | Allowed | Streaming machine-readable query adapter |
| Informational | `info`, `version`, `help`, `mod` | Allowed | Bounded structured or text adapter |
| Arbitrary execution | `run`, `mobile-install` | Disabled | BEP build phase plus bounded process output |
| Workspace mutation/network | `clean`, `shutdown`, `fetch`, `sync` | Disabled | Generic bounded result |
| Unknown future command | Any other command | Disabled | Generic bounded result when explicitly enabled |

Deployments MAY change command policy, but the tool description and rejection
message MUST make the active policy clear.

### 10.2 Query adapters

- `query` SHOULD use Bazel's streaming protobuf or NDJSON output so results can
  be processed incrementally and without loading one complete `QueryResult`.
- `cquery` and `aquery` SHOULD use the most structured output format supported by
  the detected Bazel version.
- If a caller supplies an incompatible output flag, the server rejects it with
  an actionable explanation instead of silently returning an unbounded format.
- Query rows are written incrementally to invocation storage.
- The initial result includes a count and bounded sample, never the full graph.

### 10.3 Unknown and informational output

Commands without a specialized reducer capture complete stdout and stderr to
disk and return only exit status, duration, byte counts, and a bounded text
excerpt. An explicitly enabled unknown command does not receive BEP flags unless
the detected Bazel version reports that the flags are supported for that
command.

## 11. Bazel argument and flag requirements

### 11.1 Internal flags

For build-like commands, the server owns and injects flags equivalent to:

```text
--invocation_id=<uuid>
--build_event_binary_file=<invocation-directory>/events.bep
--build_event_binary_file_path_conversion=false
--tool_tag=bazel-mcp
--color=no
--curses=no
--show_progress=false
--show_result=0
```

For test-like commands, the server additionally applies terminal-output policy
equivalent to:

```text
--test_output=summary
--test_summary=none
```

The exact supported spelling is selected from the detected Bazel version. The
server captures raw output, so these flags reduce terminal noise but are not the
only diagnostic source.

### 11.2 Reserved flag validation

- The caller MUST NOT override the invocation ID, BEP file path, or internal
  tool tag.
- Flags that would stream tests, publish all actions, or defeat bounded capture
  are rejected unless enabled by server policy.
- User arguments are preserved in their original order.
- Internal presentation and capture flags are appended in a deterministic
  position that takes precedence over `.bazelrc` terminal settings.
- The canonical post-rc command line reported by BEP is retained in redacted
  invocation metadata.

### 11.3 Prohibited silent changes

The server MUST NOT silently inject or change:

- `--remote_download_outputs`
- `--output_base`
- `--output_user_root`
- `--batch`
- remote cache or remote execution endpoints
- compilation mode, platforms, toolchains, features, defines, or test filters
- `--keep_going`
- sandboxing or spawn strategy

An agent-oriented `--remote_download_outputs=minimal` profile MAY be added later,
but it must be explicit because it changes local artifact availability. Existing
`--bes_backend` and BuildBuddy configuration MUST continue to operate.

## 12. Invocation lifecycle

### 12.1 States

An invocation has one of the following states:

```text
queued -> starting -> running -> succeeded
                              -> failed
                              -> cancelled
                              -> timed_out
                              -> interrupted
```

- `failed` means Bazel completed with a nonzero exit code.
- `cancelled` means cancellation was requested and the process stopped.
- `timed_out` means the configured execution deadline was exceeded.
- `interrupted` means the MCP server stopped or crashed before observing a
  terminal process result.

State transitions are durable and monotonic. Terminal states do not change.

### 12.2 Concurrency

- Commands using the same effective Bazel output base MUST execute serially.
- When no explicit output base is supplied, the canonical workspace is the lock
  key.
- When an explicit output base is supplied, its canonical path is the lock key,
  including across different workspaces.
- Commands for independent lock keys MAY execute concurrently subject to a
  configurable global limit.
- The default global running limit is four invocations.
- Queue position and elapsed queue time may be reported as progress.
- The server MUST NOT create alternate output bases merely to increase
  concurrency.

### 12.3 Progress

- The initial tool request MUST NOT emit child stdout or stderr.
- If the MCP request includes a progress token, the server may send one update
  after 30 seconds and no more often than every 60 seconds thereafter.
- A progress update is sent earlier only for a meaningful state transition such
  as leaving the queue or beginning execution.
- Progress messages contain only state, elapsed time, and concise structured BEP
  counts or phases when available.
- Identical progress updates are suppressed.
- Progress notification failures do not fail the Bazel invocation.

### 12.4 Timeouts and cancellation

On Unix, a running Bazel client is placed in a process group. Cancellation:

1. Sends `SIGINT` to allow the Bazel client to request graceful server-side
   cancellation.
2. Waits a configurable grace period, defaulting to 10 seconds.
3. Sends `SIGTERM` if the client remains alive.
4. Waits a second grace period, defaulting to 5 seconds.
5. Sends `SIGKILL` if necessary.

The server continues ingesting any final BEP data written during cancellation.
It MUST reap every child and MUST NOT leave an untracked Bazel client running.

## 13. Build Event Protocol ingestion

### 13.1 Capture format

- Build-like commands write a binary, varint-length-delimited BEP file.
- The complete file is retained for the configured retention period.
- The MVP MAY parse after process completion. Incremental file tailing is an
  optimization, not a requirement for initial token savings.
- A later local Build Event Service MUST NOT replace an existing remote BES
  without an explicit, tested forwarding design.

### 13.2 Protobuf management

- Bazel BEP `.proto` files and required imports are vendored at a pinned Bazel
  release tag.
- Rust owned messages and borrowed views are generated with `buffa-build`.
- Decoded events retain their protobuf frame and expose Buffa views so string,
  byte, nested-message, and repeated-message fields are not copied into a
  second owned object graph.
- The selected protobuf version MUST decode fixtures from every supported Bazel
  major version.
- Unknown protobuf fields are ignored, and missing optional fields are handled
  without failing the entire invocation.
- Version updates include golden-fixture and compatibility tests.

### 13.3 Required events

The reducer MUST understand at least:

- `BuildStarted`
- `OptionsParsed` and unstructured command line
- `PatternExpanded`
- `TargetConfigured`
- `TargetComplete` and target summaries when present
- `ActionExecuted`
- `Aborted`
- `TestResult`
- `TestSummary`
- `NamedSetOfFiles`
- `BuildFinished`
- `BuildMetrics`

The parser MUST tolerate:

- A failed or crashed Bazel process with no BEP file.
- A truncated final varint or protobuf message.
- Announced events that never arrive.
- Out-of-order references permitted by the BEP graph.
- Inline file contents, local `file://` URIs, remote URIs, and missing artifacts.
- Multiple configurations of the same label.

## 14. Reduction requirements

### 14.1 Diagnostic priority

The default failure response selects evidence in this order:

1. Loading or analysis `Aborted` events and structured failure details.
2. Failed root-cause actions and their target labels.
3. The first actionable compiler or tool error from each failed root-cause
   action.
4. Failed test cases or bounded failed-test log excerpts.
5. A bounded stderr tail when structured evidence is absent.

The response SHOULD favor root causes over cascading target failures.

### 14.2 Text normalization

Before model-visible return, text is:

- decoded lossily as UTF-8 while preserving the raw bytes on disk
- stripped of ANSI terminal sequences
- stripped of curses/progress rewrites
- normalized to replace the absolute workspace prefix with `<workspace>`
- split into logical lines
- redacted using the configured secret rules

Deduplication operates only on text that is identical after removal of ANSI,
timestamps, and the absolute workspace prefix. It MUST NOT merge merely similar
diagnostics that may refer to different source locations.

### 14.3 Default diagnostic limits

- At most 20 diagnostics per `bazel.run` response.
- At most two text diagnostics per failed action before global selection.
- At most 1,000 UTF-8 bytes per diagnostic item.
- At most two lines of leading and trailing context unless necessary to include
  a source location or explicit cause.
- Suppressed and deduplicated counts are always reported when nonzero.
- If truncation removes potentially useful data, the result identifies the
  appropriate `bazel.inspect` view.

### 14.4 Test reduction

- Aggregate passed, failed, flaky, skipped, incomplete, and cached test counts.
- Preserve run, shard, and attempt identity.
- Prefer structured `test.xml` failure names and messages when locally
  available.
- Fall back to a bounded `test.log` excerpt.
- Never include logs for passing tests in the default result.
- `include_all_results` is intentionally not part of `bazel.run`; callers use
  the paginated `tests` view instead.

### 14.5 Artifact reduction

- Resolve transitive `NamedSetOfFiles` references without quadratic expansion.
- Store artifact identity once and reference it from targets/output groups.
- Initial results report artifact counts and only artifacts required to explain
  a failure.
- Remote URIs are returned as metadata, not fetched without an integration.
- Directory outputs and symlinks are represented without recursively reading
  their contents.

### 14.6 Coverage reduction

- Discover coverage output groups and LCOV artifacts through BEP.
- Parse local LCOV data incrementally.
- Return line coverage percentage and covered/total line counts for requested
  files.
- Default coverage inspection is bounded and paginated by file path.
- If only a remote URI is available, report `remote_artifact_unavailable` and the
  artifact reference rather than silently returning zero coverage.

## 15. Response and token budgets

All limits apply to the serialized model-visible tool result, before MCP framing.

| Response | Default target | Hard requirement |
| --- | ---: | ---: |
| Successful `bazel.run` | At most 1 KiB | At most 2 KiB |
| Failed `bazel.run` | At most 4 KiB | At most 8 KiB |
| `bazel.inspect` page | At most 8 KiB | At most 32 KiB |
| Progress notification | At most 256 bytes | At most 512 bytes |

- Byte ceilings take precedence over item limits.
- Every response stopped by a ceiling sets `truncated: true` and, when possible,
  returns a continuation cursor.
- No default result includes base64 data, artifact contents, a complete BEP
  event, or a complete raw log.
- Tool descriptions and JSON schemas SHOULD remain concise because tool
  definitions may also consume model context.

## 16. Storage

### 16.1 Layout

Large data is stored in ordinary files. An embedded, in-process
[Turso](https://github.com/tursodatabase/turso) database stores metadata and
indexes in a SQLite-compatible local database file. The MVP does not require or
contact a hosted Turso service.

```text
<cache-root>/
  index.db
  workspaces/<workspace-hash>/
    invocations/<invocation-id>/
      request.json
      metadata.json
      stdout.log
      stderr.log
      events.bep
      summary.json
      artifacts.json
```

The exact platform cache root is configurable. It defaults to an OS-appropriate
user cache directory and MUST NOT be placed inside the Bazel workspace.

### 16.2 Turso database requirements

- Use the Rust `turso` crate in local mode through
  `turso::Builder::new_local`; do not use `rusqlite` or link the SQLite C
  library in production.
- Pin the pre-1.0 Turso release exactly in the workspace lockfile and upgrade it
  only with migration, crash-recovery, and pagination tests.
- Index invocations by UUID, workspace, state, start time, and finish time.
- Store normalized targets, diagnostics, tests, artifacts, query rows, and
  summary metrics in tables suitable for filtered pagination.
- Do not store complete stdout, stderr, BEP blobs, or artifact contents in
  Turso.
- Cursors encode stable ordering keys and are opaque to clients.
- Use an append-only `schema_migrations` table and run each migration in a
  transaction. Released migrations are immutable.
- Use a conservative, tested SQLite-compatible SQL subset. Configure durability
  only through behavior supported by the pinned Turso version; the
  implementation MUST NOT assume C SQLite driver or `rusqlite` behavior.
- Serialize schema and lifecycle writes inside the store while allowing bounded
  reads to proceed concurrently when the pinned Turso version supports it.

### 16.3 Durability and recovery

- Request metadata is written before the child is spawned.
- Terminal state and exit information are committed atomically.
- On startup, nonterminal invocations without a tracked live child transition to
  `interrupted`.
- Complete captured files remain inspectable after server restart.
- A partially written BEP is parsed up to the last complete message.

### 16.4 Retention

Defaults:

- Retain completed invocations for seven days.
- Limit total invocation storage to 10 GiB.
- Never evict a running invocation.
- Under quota pressure, evict the oldest terminal invocations first.
- Deleting an invocation removes its files and index rows as one recoverable
  operation.

Retention is configurable by deployment.

## 17. Security and privacy

### 17.1 Execution boundary

Bazel builds can execute repository-defined code. The MCP server is therefore a
code-execution service even though it does not expose a shell.

- Tool inputs MUST be validated.
- Workspaces MUST be allowlisted.
- Commands MUST be policy-controlled.
- Per-workspace and global rate limits MUST be enforced.
- The caller cannot set the child executable.
- The child inherits only the server's configured environment policy.

### 17.2 Filesystem permissions

- Cache directories use user-only permissions (`0700` on Unix).
- Stored files use user-only permissions (`0600` on Unix).
- Artifact references do not grant access to paths outside allowed workspaces or
  the Bazel output tree.
- Symlink targets are validated before any file is read for inspection.

### 17.3 Secret handling

- Redaction is applied before text enters Turso summaries or MCP results.
- Known credential-bearing flags and environment variables store a redacted
  placeholder in request and canonical-command metadata.
- Raw logs remain protected by filesystem permissions and retention policy but
  are still redacted when inspected.
- Telemetry MUST NOT include raw arguments, source text, compiler output, or test
  logs.

### 17.4 Transport

- Stdio is the default and required MVP transport.
- Streamable HTTP is optional after MVP.
- HTTP MUST bind to loopback by default and require authentication, host/origin
  validation, request-size limits, and session isolation.
- HTTP clients cannot access workspaces outside the server's configured policy.

## 18. Reliability and performance requirements

### 18.1 Performance

- Median Bazel wall-time overhead MUST be less than 3% on the acceptance
  benchmark.
- BEP and query processing MUST be streaming or externally stored and MUST NOT
  require memory proportional to total invocation output.
- The server SHOULD remain below 256 MiB of additional resident memory for a
  single large invocation, excluding the Bazel processes themselves.
- The server MUST remain responsive to `bazel.cancel` and `bazel.inspect` while
  other invocations run.

### 18.2 Failure isolation

- A malformed BEP event fails that invocation's structured parser, not the MCP
  server.
- A reducer panic MUST NOT terminate the process; the invocation falls back to a
  bounded captured-text result where possible.
- Failure to send progress does not cancel the build.
- Failure to write required capture files prevents process launch rather than
  running without observability.
- Disk-full errors produce an actionable tool failure and trigger retention
  cleanup without deleting running invocation data.

## 19. Compatibility

### 19.1 Initial platform matrix

- macOS on Apple Silicon and x86_64
- Linux on x86_64 and ARM64
- Bazel 7.x, 8.x, and 9.x
- Bazelisk and direct Bazel binaries
- Repository wrapper scripts that ultimately execute Bazel

Exact minor versions in continuous integration are pinned in the implementation
repository. Unsupported versions fail with a concise compatibility message and
may use the generic bounded-text adapter only when explicitly configured.

### 19.2 MCP compatibility

- Implement the stable MCP protocol version selected by the pinned official Rust
  SDK.
- Support standard tools, progress, and cancellation.
- Advertise task-augmented execution as optional only when conformance-tested.
- Stdio output is reserved exclusively for MCP protocol messages; application
  logs go to stderr or configured files without corrupting MCP framing.

## 20. Observability and measurement

For every invocation, record:

- Raw stdout bytes
- Raw stderr bytes
- BEP bytes and event count
- Model-visible result bytes
- Number of progress notifications
- Number of inspect calls
- Queue time, Bazel wall time, and reduction time
- Counts of diagnostics returned, suppressed, and deduplicated
- Parser fallback reasons
- Bazel and reducer versions

The service can measure bytes and calls but cannot infer provider billing
tokens. The integration harness MUST also produce deterministic token estimates
with [`tiktoken-rs`](https://github.com/zurawiki/tiktoken-rs). It tokenizes the
canonical model-visible transcript with `o200k_base` by default and records the
crate version and encoding in every report. The encoding is configurable so the
same transcript can be evaluated for a different OpenAI model family.

For each run, the offline report includes:

- `visible_tool_tokens`: the sum of tokens in canonical tool-result payloads.
- `cumulative_context_tokens`: the sum, over model events, of tokens in the
  canonical context visible immediately before that event. This deliberately
  charges polling for repeatedly exposing prior context.
- Raw and model-visible bytes, tool calls, polling calls, model events, Bazel
  wall time, and diagnostic-fidelity outcome.

These are tokenizer-based comparative estimates, not claims about provider
billing, cache discounts, hidden prompts, or a non-OpenAI tokenizer. When the
target agent platform exposes them, the harness MUST additionally obtain actual
input, uncached input, cached input, output token, and model-event measurements.

No telemetry leaves the machine unless a deployment explicitly configures an
external sink.

## 21. Acceptance criteria

### 21.1 Token-efficiency gates

Against an equivalent terminal-tool baseline:

- At least 75% lower aggregate `cumulative_context_tokens` across the pinned
  integration benchmark suite.
- When actual platform usage is available, at least 75% lower total model input
  tokens across the same suite is the production validation gate.
- At least 75% fewer model-visible Bazel output bytes.
- No more than one model-visible execution result for a completed invocation on
  a task-capable or long-request-capable host.
- No periodic agent polling in the normal `bazel.run` path.
- Successful and failed results satisfy the response budgets in section 15.
- An 85% token reduction is a stretch goal, not a release gate.

### 21.2 Diagnostic-fidelity gates

For every benchmark failure, either:

1. The default result contains the actionable root cause, or
2. The default result identifies the correct narrow inspection view and the root
   cause is available in at most one `bazel.inspect` call.

The corpus MUST include:

- Workspace and target-pattern errors
- Loading errors
- Analysis and visibility failures
- C++, Java, Rust, and one custom-action compilation failure
- Many repeated warnings before an error
- Multiple independent failed actions with and without `--keep_going`
- Passed, failed, flaky, cached, sharded, retried, and timed-out tests
- A failed test with a very large log
- A Bazel crash or killed client with a partial BEP
- A remote action failure whose referenced artifact is unavailable locally
- Coverage with local LCOV and coverage with only a remote reference

### 21.3 Concurrency and lifecycle gates

- Two commands for the same workspace execute serially without relying on
  Bazel's invisible output-base lock wait.
- Independent workspaces can run concurrently.
- Two worktrees cannot read each other's "last" result because no such API
  exists.
- Cancellation stops queued work immediately and running work within the defined
  escalation window.
- Server restart changes orphaned invocations to `interrupted` and retains their
  captured evidence.
- Retention never deletes running invocation data.

### 21.4 Performance gates

- Less than 3% median Bazel wall-time overhead.
- Bounded memory on a query producing at least one million rows.
- No quadratic artifact expansion for nested `NamedSetOfFiles` graphs.
- No MCP server crash on malformed or truncated fixture input.

### 21.5 Safety gates

- Shell metacharacters in arguments are passed literally and never evaluated.
- Workspace symlinks cannot escape allowed roots for inspection.
- Reserved internal flags cannot be overridden.
- `clean`, `shutdown`, and `run` are rejected under the default policy.
- Secrets in configured redaction fixtures do not appear in summaries,
  inspection responses, Turso text columns, or telemetry.

## 22. Test strategy

### 22.1 Unit tests

- Varint framing split at every possible byte boundary
- Unknown and missing protobuf fields
- Truncated streams
- BEP graph references arriving before and after the referenced event
- Nested and repeated named file sets
- Diagnostic normalization and exact deduplication
- Redaction
- Pagination and cursor stability
- Response byte ceilings
- State-machine transition validity
- Command and flag policy

### 22.2 Golden fixtures

Check in redacted BEP and log fixtures for every supported Bazel major version and
failure class. Expected summaries are reviewed as product artifacts. Updating a
reducer requires an explicit golden diff.

### 22.3 End-to-end harness

Create small Bazel workspaces that deterministically exercise success, loading,
analysis, compilation, test, coverage, query, timeout, cancellation, concurrency,
and remote-artifact cases. Tests run through the MCP interface, not only the Rust
library API.

### 22.4 Agent benchmark

Run identical repository tasks under:

1. A normal terminal/shell tool baseline.
2. The optimized shell instructions described in the source discussion.
3. `bazel-mcp`.

Record model events, tool calls, polling calls, input tokens, uncached input
tokens, output tokens, model-visible bytes, credits/cost when available, Bazel
wall time, and whether the agent found the correct root cause.

The required public integration corpus is
[`abseil/abseil-cpp`](https://github.com/abseil/abseil-cpp), using release tag
`20260526.0` at full commit
`5650e9cf76d3be4318d5fa3af38ee483ddfd5e4a`. The harness MUST:

1. Fetch into an ignored cache only through an explicit setup command, verify
   the exact commit, and never benchmark a moving branch or tag resolution.
2. Create a clean disposable worktree or copy for every sample and apply only
   repository-owned scenario overlays; it MUST NOT modify the cached source.
3. Include at least a successful build, a deterministic C++ compile failure, a
   deterministic failed test with a bounded actionable message, a deliberately
   noisy failing action with repeated lines, and a representative query.
4. Run each scenario through the direct terminal, optimized terminal, and MCP
   adapters with the same Bazel version, target, flags, environment, cache
   condition, and task text.
5. Isolate each adapter's Bazel output user root. Report cold and warm-cache
   results separately, randomize adapter order, run at least five measured
   samples after warm-up, and report median and p95 wall time and token counts.
6. Persist raw process evidence separately from a canonical JSONL transcript.
   Token accounting MUST include only content actually exposed to the model,
   including progress or polling payloads when an adapter exposes them.
7. Tokenize canonical transcript fields with `tiktoken-rs`; default to the
   reusable `o200k_base` tokenizer and fail rather than silently substituting an
   unknown encoding.
8. Produce machine-readable JSON and a reviewed Markdown summary containing
   absolute counts, reduction percentages, diagnostic-fidelity results,
   environment metadata, project commit, Bazel version, tokenizer version, and
   encoding.

The deterministic offline run is the repeatable integration test and requires
no model credentials. A live-agent mode may replay the same tasks to validate
actual provider metrics, but paid live runs are manual or scheduled and are not
a normal pull-request gate.

## 23. Implementation architecture

The Rust workspace SHOULD separate protocol, execution, storage, and reduction:

```text
crates/
  bazel-mcp-server/      MCP handlers and transport
  bazel-runner/          workspace discovery, scheduling, child lifecycle
  bazel-bep/             generated protos, framing, event graph
  bazel-reducer/         build, test, coverage, query, and text reducers
  bazel-store/           Turso index, files, retention, cursors
  bazel-policy/          commands, flags, roots, redaction
```

Expected foundational dependencies include:

- `rmcp`
- `tokio` and `tokio-util`
- `buffa` and `buffa-build`
- `serde`, `serde_json`, and `schemars`
- `uuid`
- `turso` in embedded local mode
- `tracing`
- `thiserror`
- an LCOV parser or a small internal streaming parser
- an XML parser for bounded `test.xml` extraction

The MCP handler MUST delegate execution to an invocation manager. It MUST NOT
hold a global mutex across a running build or perform blocking file I/O on a
Tokio core worker. Turso operations are awaited through the store's async API;
blocking BEP and filesystem work uses bounded blocking tasks.

## 24. Delivery plan

### Phase 0: Measurement and fixtures

- Build the terminal baseline and agent benchmark harness.
- Pin the Abseil corpus and implement deterministic `tiktoken-rs` transcript
  accounting before optimizing reducers.
- Collect representative BEP and failure fixtures.
- Establish model-visible byte and token accounting.

### Phase 1: Executable alpha

- Stdio MCP server with the three tools.
- Workspace and executable policy.
- Durable invocation IDs and file capture.
- Per-workspace scheduling, timeout, and cancellation.
- Post-completion BEP parsing for build and test.
- Default summary and bounded log fallback.

### Phase 2: MVP

- Full reducers for required BEP events.
- Test XML extraction.
- Coverage and query adapters.
- Turso filtering, migrations, recovery, and stable pagination.
- Retention and crash recovery.
- Token, diagnostic-fidelity, performance, and security acceptance gates.
- macOS/Linux and Bazel-version compatibility matrix.

### Phase 3: Hardening and integrations

- Incremental BEP parsing when it provides useful progress.
- Streamable HTTP with authentication.
- Remote CAS and BuildBuddy adapters.
- Language-specific diagnostic parsers.
- Windows process and job-object support.

## 25. Risks and mitigations

| Risk | Impact | Mitigation |
| --- | --- | --- |
| Compiler diagnostics are tool-specific text | Root cause may be missed | Use BEP to select failed actions, bounded language-aware extractors, and preserved raw-log fallback. |
| Long MCP calls time out in some hosts | Reintroduces polling | Support optional MCP tasks, progress, configurable timeouts, and conformance tests per host. |
| BEP schema changes across Bazel versions | Parse or semantic drift | Pin protos, ignore unknown fields, maintain cross-version golden fixtures. |
| Remote-minimal builds reference unavailable artifacts | Test or coverage detail is missing | Report availability explicitly; add authenticated CAS/BuildBuddy adapters later. |
| Tool results duplicate text and structured content | Token savings regress | Default to one text representation and benchmark host-specific encoding. |
| Concurrent commands wait on the same output-base lock | Hidden latency and apparent stalls | Schedule by canonical effective output base before spawning Bazel. |
| Logs contain secrets | Sensitive data reaches model or disk | Private storage, retention, configured redaction before summaries and inspection. |
| Retained logs consume large amounts of disk | Builds fail or machine degrades | Quotas, oldest-first eviction, and preflight storage checks. |
| The pinned pre-1.0 Turso release changes behavior or lacks a SQLite edge case | Metadata migration or query failure | Pin the release, use a conservative SQL subset, test migrations/crash recovery, and preserve raw invocation files so indexes can be repaired. |
| Token estimates are mistaken for provider billing | Savings claims are misleading | Label `tiktoken-rs` results as deterministic estimates, record the encoding, and corroborate with live platform metrics when available. |
| The upstream benchmark corpus drifts or disappears | Results cease to be reproducible | Pin Abseil by full commit, verify the checkout, cache it, and keep scenario overlays in this repository. |
| Quiet flags hide the only useful error | Agent cannot diagnose failure | Capture complete stdout/stderr and retain BEP before applying reduction. |
| Server silently changes build behavior | Incorrect or surprising results | Reserve changes to presentation/capture flags; prohibit semantic flag injection. |

## 26. Deferred decisions

These decisions do not block the MVP requirements:

- Which production MCP hosts should use `structured` rather than `text` result
  encoding after token benchmarks.
- Whether incremental BEP file tailing is reliable enough across all supported
  Bazel versions to become the default progress source.
- Which remote CAS and BuildBuddy authentication mechanisms to support first.
- Whether a future explicit agent profile should enable
  `--remote_download_outputs=minimal`.
- Whether invocation resources should be subscribable in addition to being
  inspectable through tools.

## 27. References

- [Bazel Build Event Protocol](https://bazel.build/remote/bep)
- [Bazel BEP glossary](https://bazel.build/remote/bep-glossary)
- [Bazel command-line reference](https://bazel.build/reference/command-line-reference)
- [Bazel query output formats](https://bazel.build/query/language#output-formats)
- [Bazel `build_event_stream.proto`](https://github.com/bazelbuild/bazel/blob/master/src/main/java/com/google/devtools/build/lib/buildeventstream/proto/build_event_stream.proto)
- [MCP 2025-11-25 specification](https://modelcontextprotocol.io/specification/2025-11-25)
- [MCP tools specification](https://modelcontextprotocol.io/specification/2025-11-25/server/tools)
- [Official Rust MCP SDK](https://github.com/modelcontextprotocol/rust-sdk)
- [Turso in-process SQL database](https://github.com/tursodatabase/turso)
- [`tiktoken-rs`](https://github.com/zurawiki/tiktoken-rs)
- [Abseil C++](https://github.com/abseil/abseil-cpp)
