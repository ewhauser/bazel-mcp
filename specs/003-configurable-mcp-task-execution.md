# 003: Negotiated MCP Task Execution

| Field | Value |
| --- | --- |
| Status | Implemented |
| Specification | 003 |
| Product | `bazel-mcp` |
| Last updated | 2026-07-15 |

## Summary

Add negotiated task execution for `bazel.run`. One server supports synchronous
execution, the experimental task protocol from MCP `2025-11-25`, and the
`io.modelcontextprotocol/tasks` extension defined by SEP-2663. It selects the
wire contract at runtime from the negotiated protocol version and the client
capabilities carried by the request.

A deployment-level `mcp_execution_policy` setting has exactly three values:
`auto`, `sync_only`, and `tasks_required`. The policy controls whether a
compatible request is deferred; it does not select or translate a task
dialect.

The two task protocols are not wire-compatible and MUST NOT be advertised in
the same negotiated protocol context. A server process MAY support both and
dispatch between them as required by SEP-2663's backward-compatibility
guidance. A task response is emitted only when the request declares support for
the selected dialect.

Task support does not add MCP tools. The server continues to expose exactly
`bazel.run`, `bazel.inspect`, and `bazel.cancel`. Task methods are MCP protocol
methods, not tools.

## Relationship to specifications 001 and 002

This specification amends the `bazel.run` execution behavior in specification
001. The statement that task support is optional is replaced by the negotiated
policy behavior in this document. All tool inputs, logical run results, response
budgets, evidence retention, command policy, cancellation escalation, and the
three-tool limit remain unchanged.

Specification 002 remains authoritative for crate boundaries and dependency
direction. In particular:

- only `bazel-mcp-server` depends on `rmcp`;
- store APIs use domain types and do not expose MCP wire types;
- `InvocationService` remains the only application boundary presented to the
  server; and
- database-free filesystem records remain the production storage layer.

## Motivation

Bazel invocations may outlive an MCP host's preferred request duration. A task
handle lets the host release the original request, continue other work, and
retrieve the bounded result later. It also makes a disconnect less likely to
discard the invocation identity.

MCP currently has two materially different task designs. The MCP SDK embedded
in Claude Code recognizes the legacy schema, but the pinned Claude Code host
does not opt into task execution. Newer MCP work moves tasks into an extension
with a different negotiation and result flow. The protocol already provides
enough information to distinguish the wire contracts: legacy tasks are selected
by protocol `2025-11-25`, initialization capabilities, tool metadata, and
`params.task`; extension tasks are selected by a newer protocol version and the
per-request extension capability.

The implementation therefore negotiates the wire contract and leaves
configuration to express operator policy. This permits one deployment to serve
legacy, extension-aware, and synchronous-only hosts without guessing from host
names or silently translating between protocols.

Task support must remain storage-efficient. It does not require a second copy of
stdout, stderr, BEP, or the final model-visible response. A small deferred-result
record points at the existing durable invocation and its bounded summary.

## Goals

- Select the asynchronous response shape deterministically from negotiated
  protocol information.
- Implement both the `2025-11-25` legacy protocol and the SEP-2663 extension
  according to their distinct wire contracts.
- Return a task handle only after `tasks/get` can resolve it durably.
- Reuse one invocation ID as the task ID and execute each accepted request once.
- Reconstruct the same bounded `CallToolResult` used by synchronous execution.
- Keep task status, results, and cancellation available across a server restart.
- Keep task metadata compact and independent from raw evidence retention.
- Test every policy and negotiated flow at the stdio wire boundary.
- Test synchronous fallback and task-policy rejection through a pinned Claude
  Code executable, not only through an SDK client.

## Non-goals

- Choosing behavior from host names, executable paths, or undocumented client
  heuristics.
- Translating a task handle from one wire dialect into the other.
- Adding `bazel.task_status`, `bazel.task_result`, or any other MCP tool.
- Resuming the Bazel child process after the server process itself crashes.
- Supporting task-hosted sampling, elicitation, or arbitrary task input in the
  first implementation.
- Adding HTTP transport or a multi-tenant authorization model.
- Changing the logical `bazel.run` result or its byte budgets.
- Retaining raw logs for the lifetime of a task solely because a task exists.
- Removing successful invocation evidence. That would change specification
  001's evidence-preservation policy and requires a separate decision.

## Terminology and protocol baselines

- **Synchronous result:** the `CallToolResult` returned directly by
  `tools/call`.
- **Deferred result:** protocol-neutral durable metadata linking a task-shaped
  response to an invocation and bounded result.
- **Legacy tasks:** the experimental task utility in MCP `2025-11-25`. See the
  [legacy Tasks specification](https://modelcontextprotocol.io/specification/2025-11-25/basic/utilities/tasks).
- **Tasks extension:** `io.modelcontextprotocol/tasks` as accepted by SEP-2663.
  See [SEP-2663](https://modelcontextprotocol.io/seps/2663-tasks-extension) and
  the [extension overview](https://modelcontextprotocol.io/extensions/tasks/overview).
- **Task ID:** the string placed in MCP task messages. In this product it is the
  invocation UUID rendered as a string.
- **Acceptance commit point:** the durable transaction after which the server
  may return a task handle and the invocation will continue independently of the
  original request.

The implementation pins both task dialects in
`specs/fixtures/mcp-task-protocols.json`. The accepted SEP-2663 baseline is the
final Tasks extension at its `2026-06-30` base protocol and the exact upstream
schema commit recorded there. Extension behavior is never derived from
`rmcp::ProtocolVersion::LATEST`. Any upstream date or schema change requires a
reviewed golden diff.

## Configuration

### Execution policy

`ServerConfig` gains:

```toml
mcp_execution_policy = "auto"
```

The Rust type is:

```rust
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpExecutionPolicy {
    #[default]
    Auto,
    SyncOnly,
    TasksRequired,
}
```

The policy is read only at startup. Changing it requires a server restart.
There is no per-call policy override. Protocol version and capability
negotiation are inputs to the policy, not host detection.

- `auto` is the default. A compatible extension request is deferred. A legacy
  request is deferred when it includes `params.task`. Other requests execute
  synchronously.
- `sync_only` never creates a new task and always returns the ordinary
  synchronous result. Previously accepted, unexpired task handles remain
  readable and cancellable after a policy change.
- `tasks_required` requires a compatible task capability for `bazel.run` and
  returns the dialect-specific capability or protocol error when it is absent.

The server advertises task read and cancellation surfaces that are valid for the
negotiated protocol version even when `sync_only` prevents new task creation.
This keeps existing durable handles usable after restart. Under the legacy
protocol, `auto` and `tasks_required` advertise
`capabilities.tasks.requests.tools.call`; `sync_only` omits it but MAY continue
to advertise `tasks.list` and `tasks.cancel`. Correspondingly,
`bazel.run.execution.taskSupport` is `optional`, absent, or `required` for
`auto`, `sync_only`, or `tasks_required`.

Unknown values are startup errors. The error is written to stderr and the
process exits nonzero before writing any MCP frame to stdout.

### Task lifecycle settings

`ServerConfig` also gains:

```toml
task_ttl_seconds = 86400
task_poll_interval_ms = 2000
```

- `task_ttl_seconds` is the minimum terminal-result availability window. It
  defaults to 24 hours and MUST be greater than zero.
- `task_poll_interval_ms` is returned as the suggested poll interval. It
  defaults to 2,000 ms and MUST be between 100 and 60,000 inclusive.
- Both fields are validated under every policy so a configuration can be
  promoted between environments without discovering an invalid value only
  after a policy switch.
- They have no effect on newly executed calls under `sync_only`.
- A legacy caller's requested `task.ttl` is advisory. The server returns and
  persists its configured actual TTL instead of accepting a shorter retention
  window that could expire a queued build.
- A nonterminal task is never purged. On terminal transition, its expiry is
  extended when necessary so the final result remains available for at least
  `task_ttl_seconds`. The advertised TTL is updated to the actual duration from
  creation.

The example configuration and README MUST document the policy, compatibility
expectations, TTL, and poll interval.

## Negotiated execution contract

| Negotiated flow | Selection | `bazel.run` response | Result retrieval |
| --- | --- | --- | --- |
| Synchronous | no compatible task request, or `sync_only` | `CallToolResult` | original `tools/call` |
| Legacy tasks | protocol `2025-11-25` plus `params.task` | nested `CreateTaskResult.task` | `tasks/result` |
| Tasks extension | extension-capable protocol plus per-request `io.modelcontextprotocol/clientCapabilities` | flat `CreateTaskResult` with `resultType: "task"` | terminal `tasks/get.result` |

| Request | `auto` | `sync_only` | `tasks_required` |
| --- | --- | --- | --- |
| Extension-capable `bazel.run` | extension task | synchronous | extension task |
| Modern request without extension capability | synchronous | synchronous | `-32003` |
| Legacy `bazel.run` with `params.task` | legacy task | synchronous | legacy task |
| Legacy `bazel.run` without `params.task` | synchronous | synchronous | `-32601` |
| Earlier protocol without task support | synchronous | synchronous | `-32601` |

`bazel.inspect` and `bazel.cancel` always return ordinary synchronous
`CallToolResult` values. They never create tasks under any policy or negotiated
flow.

### Synchronous execution

When policy negotiation selects synchronous execution, `bazel.run` performs the
current flow:

1. validate and durably create the invocation;
2. wait for its terminal record while servicing bounded progress
   notifications; and
3. return the encoded `CallToolResult`.

Under `sync_only`, a legacy `params.task` value is ignored under the legacy
specification's non-declaring receiver rule because the server omits
`capabilities.tasks.requests.tools.call`. A client extension capability also
does not force task creation. The server returns a normal result.

Cancellation of the original `tools/call` maps to the invocation cancellation
token as it does today.

### Legacy tasks

This flow implements the MCP `2025-11-25` task design.

#### Negotiation and tool discovery

The legacy adapter is active only after negotiation of protocol `2025-11-25`.
Other protocol versions never receive legacy task response shapes.

Under `auto` and `tasks_required`, the initialize result advertises:

```json
{
  "capabilities": {
    "tasks": {
      "list": {},
      "cancel": {},
      "requests": {
        "tools": {
          "call": {}
        }
      }
    }
  }
}
```

`tools/list` contains exactly the existing three tools. Under `auto`,
`bazel.run` includes:

```json
{
  "execution": {
    "taskSupport": "optional"
  }
}
```

`bazel.inspect` and `bazel.cancel` omit task support. Under `auto` and
`tasks_required`, a task-augmented call to either is rejected with
`-32601 Method not found`. Under `sync_only`, the server has not declared task
support for `tools/call`, so it ignores task augmentation metadata and processes
either tool normally.

#### Creating and reading a task

A `bazel.run` call creates a legacy task only when it includes `params.task`.
Under `auto`, absence selects synchronous execution. Under `tasks_required`,
absence is rejected with `-32601 Method not found`. After validation and the
acceptance commit point, the deferred response is:

```json
{
  "task": {
    "taskId": "<invocation-uuid>",
    "status": "working",
    "createdAt": "<iso-8601>",
    "lastUpdatedAt": "<iso-8601>",
    "ttl": 86400000,
    "pollInterval": 2000
  },
  "_meta": {
    "io.modelcontextprotocol/model-immediate-response":
      "Bazel invocation <invocation-uuid> is running.",
    "io.modelcontextprotocol/related-task": {
      "taskId": "<invocation-uuid>"
    }
  }
}
```

The immediate-response string is optional at the protocol level but required by
this product. It is bounded to 128 UTF-8 bytes, contains no command arguments or
logs, and is subject to redaction before persistence or transmission.

The server implements:

- `tasks/get` for the current state;
- `tasks/result` for the original `CallToolResult`;
- `tasks/list` with opaque cursor pagination over nonexpired deferred
  invocations, newest first; and
- `tasks/cancel` when the task is nonterminal.

`tasks/result` blocks until the task is terminal, then returns the same encoded
tool content and `isError` value as synchronous execution. The returned
`CallToolResult._meta` additionally contains
`io.modelcontextprotocol/related-task` with the task ID. Cancelling a blocked
`tasks/result` request cancels only that wait; it does not cancel Bazel.

`tasks/cancel` requests runner cancellation, waits for the runner's bounded
cancellation escalation and terminal invocation record, makes the final tool
result retrievable, durably sets the task state to `cancelled`, and then returns
the cancelled Task object. Repeated cancellation of a terminal task returns
`-32602 Invalid params`, as required by the legacy specification. The task
remains `cancelled` even if the child raced to another terminal invocation
state.

Correctness relies on polling. `notifications/tasks/status` MAY be added as a
best-effort optimization after polling conformance passes, but it is not part of
the initial acceptance criteria.

### Tasks extension

This flow implements the `io.modelcontextprotocol/tasks` extension from
SEP-2663. In a protocol context where the extension is defined, the server MUST
NOT advertise legacy `capabilities.tasks` fields or `execution.taskSupport`
values.

#### Discovery and per-request capability

`server/discover` advertises:

```json
{
  "capabilities": {
    "extensions": {
      "io.modelcontextprotocol/tasks": {}
    }
  }
}
```

Every extension request MUST carry:

```json
{
  "_meta": {
    "io.modelcontextprotocol/clientCapabilities": {
      "extensions": {
        "io.modelcontextprotocol/tasks": {}
      }
    }
  }
}
```

Under `auto` and `tasks_required`, `bazel.run` elects to create a task when the
request carries the extension capability. Under `sync_only`, it returns a
synchronous result even when the capability is present, which SEP-2663 permits.

If the client omits the per-request capability, `auto` and `sync_only` return a
synchronous result. `tasks_required` returns `-32003 Missing Required Client
Capability` with `requiredCapabilities.extensions.io.modelcontextprotocol/tasks`
in the error data and MUST NOT run Bazel.

A legacy `params.task` field is treated as an unknown field and ignored. It does
not opt the request into the extension.

#### Creating and reading a task

After validation and the acceptance commit point, `bazel.run` returns the flat
polymorphic shape:

```json
{
  "resultType": "task",
  "taskId": "<invocation-uuid>",
  "status": "working",
  "createdAt": "<iso-8601>",
  "lastUpdatedAt": "<iso-8601>",
  "ttlMs": 86400000,
  "pollIntervalMs": 2000
}
```

`tasks/get` returns `resultType: "complete"`. A working response contains task
metadata. A completed response additionally contains `result` with the original
`CallToolResult`. A failed response contains a JSON-RPC `error`. A cancelled
response contains neither.

`tasks/cancel` validates the task ID, records cancellation intent, requests
runner cancellation, and returns an empty result with
`resultType: "complete"`. The acknowledgement is not a terminal-state promise.
The task may remain `working` until cancellation is observed, or become
`completed` if the invocation won the race.

`tasks/update` is recognized so the extension method surface is complete.
`bazel.run` never enters `input_required` in this milestone. For a known task,
input responses are ignored and an empty `resultType: "complete"`
acknowledgement is returned. An unknown or expired task ID returns `-32602`.

`tasks/result` and `tasks/list` are not part of this extension and return
`-32601 Method not found`.

Task notifications and `subscriptions/listen` are optional in SEP-2663 and are
not implemented initially. Clients poll `tasks/get`.

### Progress and task association

Synchronous execution retains the existing bounded `notifications/progress`
behavior when the caller supplies a progress token.

The legacy task flow does not emit progress notifications in the first milestone.
Status is available through `tasks/get`. If progress is enabled later, the
original progress token remains valid for the task lifetime and every
task-associated message MUST include the legacy
`io.modelcontextprotocol/related-task` metadata where required by the legacy
specification. `tasks/get`, `tasks/list`, and `tasks/cancel` use their explicit
task IDs and do not add redundant related-task metadata.

The extension task flow does not emit `notifications/progress` for task
execution. The extension uses `tasks/get` and, in a future notification
milestone, `notifications/tasks`.

## Shared invocation lifecycle

### Submission API

`InvocationService` is split into submission and observation without exposing
MCP concepts:

```rust
pub async fn submit(
    &self,
    request: InvocationRequest,
    disposition: ResultDisposition,
) -> Result<InvocationId, RunnerError>;

pub async fn wait(
    &self,
    id: InvocationId,
    cancellation: CancellationToken,
) -> Result<InvocationRecord, RunnerError>;

pub async fn deferred_result(
    &self,
    id: InvocationId,
) -> Result<DeferredResultView, RunnerError>;
```

`ResultDisposition` is `Attached` or
`Deferred { retrieval, expires_at }`. `retrieval` is a protocol-neutral
`DeferredRetrieval::SeparateResult` or `DeferredRetrieval::InlineResult`. These
are domain concepts in `bazel-mcp-types`; they contain no `rmcp` types or MCP
field names.

Legacy task submissions use `SeparateResult` and extension task submissions use
`InlineResult`. Each adapter treats the other retrieval kind as an unknown task.
The retrieval kind, not the current execution policy, selects the read adapter
after restart and prevents translating an existing handle into another dialect.

The existing synchronous `run_with_cancellation` becomes a convenience
composition of `submit(Attached)` and `wait`. Both task flows call
`submit(Deferred)` and return after the durable commit.

Submission performs all request parsing, workspace and policy validation, and
reserved-argument checks before the acceptance commit point. Validation errors
are returned synchronously and create neither a task nor an invocation.

The runner, not the original RPC future, owns a deferred invocation's execution
cancellation token. Dropping the `tools/call` future or disconnecting the stdio
client after the acceptance commit point does not cancel the invocation.

### Acceptance transaction and exactly-once behavior

The acceptance transaction:

1. allocates one UUIDv7 invocation ID;
2. inserts the invocation's requested state;
3. inserts its deferred-result row when applicable; and
4. commits before the task response is serialized.

After commit, `tasks/get` can resolve the task even if it is still queued.
Failure to enqueue after commit is durably materialized as a terminal tool error;
it is not retried as a second invocation.

The task ID is exactly the invocation ID. There is no mapping table and no
second public identifier.

MCP does not provide a stable idempotency key for a lost `CreateTaskResult`.
Repeating the original `tools/call` creates a new invocation. Legacy
`tasks/list` can help recover a lost handle; the extension intentionally has no
list operation. This limitation is documented and counted as an orphaned-handle
metric.

### Restart recovery

Deferred metadata and terminal summaries survive restart. On startup:

- terminal deferred invocations remain queryable until expiry;
- a nonterminal invocation orphaned by process loss transitions to
  `interrupted` according to specification 001;
- the task then exposes a bounded final `CallToolResult` with
  `state: "interrupted"` rather than rerunning Bazel; and
- no task causes an automatic second Bazel invocation.

A handle remains available across restart until expiry regardless of an
`mcp_execution_policy` change. The policy controls creation only. Task reads and
cancellation dispatch from the negotiated protocol and the stored retrieval
kind. Protocol handles are never translated between dialects.

## Durable data model

### Domain types

`bazel-mcp-types` gains protocol-neutral types:

```rust
pub struct DeferredResultRecord {
    pub invocation_id: InvocationId,
    pub retrieval: DeferredRetrieval,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub expires_at_ms: i64,
    pub cancellation_requested_at_ms: Option<i64>,
    pub terminal_override: Option<DeferredTerminalState>,
    pub failure: Option<DeferredFailure>,
}

pub enum DeferredTerminalState {
    Cancelled,
}

pub enum DeferredRetrieval {
    SeparateResult,
    InlineResult,
}

pub struct DeferredFailure {
    pub kind: DeferredFailureKind,
    pub redacted_message: String,
}
```

The server maps these records and the associated `InvocationRecord` into the
selected protocol's Task type. Protocol-specific distinctions such as legacy
`failed` for `isError: true` remain in `bazel-mcp-server`.

### Filesystem record

Deferred metadata is an optional typed field in the invocation's versioned
`record.json`. Invocation acceptance writes request metadata and this deferred
handle before the record rename commits the submission. Startup rebuilds the
retrieval and expiry indexes from records; there is no database migration or
legacy schema.

Store methods are named for deferred results and accept domain types. They do
not mention MCP, task wire types, or `rmcp`.

Required operations are:

- create a deferred result in the invocation acceptance commit;
- read one deferred result joined with its invocation;
- require the selected adapter's retrieval kind on every task read;
- page nonexpired deferred results for legacy listing;
- record cancellation intent;
- atomically set a terminal cancellation override;
- persist a redacted internal failure;
- extend terminal expiry; and
- clear expired deferred-result metadata.

### Storage and retention behavior

Task support stores only compact lifecycle metadata. It MUST NOT copy stdout,
stderr, BEP, artifacts, or `CallToolResult` payloads.

The final result is deterministically reconstructed from the retained invocation
record and summary through the same server-side result builder used by
synchronous execution.
The configured `result_encoding` applies when the result is retrieved.

An unexpired deferred result protects the small invocation record and bounded
summary needed to reproduce the final result. It does not extend raw evidence
retention. Raw files remain governed by `retention_days`,
`maximum_storage_bytes`, and the evidence policy in specification 001.
`bazel.inspect` may therefore report that an old raw view has expired while the
task's bounded final result is still available.

Expiry is enforced lazily on task reads and by the existing retention pass.
Expired IDs return `-32602` and are omitted from legacy listing. Expiry and
storage-pressure deletion MUST NOT delete a live invocation or its minimal
summary.

## Result and status mapping

One pure `RunResultBuilder` in `bazel-mcp-server` converts an
`InvocationRecord` into the logical JSON result, applies the existing success or
failure byte ceiling, records model-visible byte metrics, and encodes the
configured `CallToolResult`. Synchronous execution, `tasks/result`, and terminal
`tasks/get.result` all use it.

| Invocation/deferred condition | Legacy Task | Extension Task | Final tool result |
| --- | --- | --- | --- |
| requested, queued, starting, or running | `working` | `working` | unavailable |
| Bazel exit 0 | `completed` | `completed` | `isError: false`, `state: succeeded` |
| Bazel exit nonzero | `completed` | `completed` | `isError: false`, `state: failed` |
| timeout with bounded summary | `completed` | `completed` | `isError: false`, `state: timed_out` |
| interrupted with bounded summary | `completed` | `completed` | `isError: false`, `state: interrupted` |
| accepted tool execution error | `failed` | `completed` | `isError: true` |
| legacy `tasks/cancel` accepted | `cancelled` immediately | not applicable | legacy result remains retrievable |
| extension cancellation observed | not applicable | `cancelled` | no result field |
| extension cancellation loses race | not applicable | `completed` | ordinary final result |
| unrecoverable internal protocol failure | `failed` | `failed` | persisted redacted error |

A nonzero Bazel exit is never a protocol task failure. It is a completed tool
call whose logical invocation state is `failed`.

Status messages are generated from small enumerated templates. They contain
state and elapsed time only, are capped at 256 UTF-8 bytes, and never contain
raw logs, command arguments, environment values, or unredacted error text.

## Cancellation and race semantics

All cancellation entry points converge on
`InvocationService::cancel(invocation_id)` and the existing process-group
escalation:

- cancellation of an attached synchronous `tools/call`;
- the `bazel.cancel` tool;
- legacy `tasks/cancel`; and
- extension `tasks/cancel`.

They differ only in protocol acknowledgement and task-state timing.

The following races have explicit outcomes:

- Cancellation before the acceptance transaction aborts submission and creates
  no task.
- Cancellation after the acceptance transaction does not revoke the task
  handle.
- Legacy cancellation waits for the final bounded tool result, then commits
  `cancelled` before replying.
- Extension cancellation records intent and acknowledges before termination;
  the observed terminal invocation wins.
- `bazel.cancel` returns its existing tool result. A related task remains
  `working` until the runner observes a terminal state.
- Cancelling a `tasks/get` or `tasks/result` wait never cancels the underlying
  invocation.
- Cancellation is idempotent inside the runner even where a wire protocol
  requires the second request to return an error.

## Protocol error matrix

| Condition | Response |
| --- | --- |
| `tasks_required` legacy `bazel.run` without `params.task` | `-32601 Method not found` |
| Legacy task metadata on `bazel.inspect` or `bazel.cancel` under `auto` or `tasks_required` | `-32601 Method not found` |
| `tasks_required` modern `bazel.run` without per-request extension capability | `-32003 Missing Required Client Capability` |
| Extension task method without per-request capability | `-32003 Missing Required Client Capability` |
| Unknown or expired task ID | `-32602 Invalid params` |
| `tasks/result` in an extension protocol context | `-32601 Method not found` |
| `tasks/update` in a legacy protocol context | `-32601 Method not found` |
| `tasks/list` in an extension protocol context | `-32601 Method not found` |
| Second legacy cancellation of a terminal task | `-32602 Invalid params` |
| Extension cancellation of a known terminal task | empty acknowledged result |
| Invalid `bazel.run` arguments before acceptance | ordinary immediate tool error; no task |
| Internal failure after acceptance | durable terminal task failure/result according to the status mapping |

Errors and stored failure messages are redacted before transmission,
persistence in durable metadata, or telemetry.

## Server architecture

### Negotiated adapters

`bazel-mcp-server` keeps three response adapters behind a protocol dispatcher:

```text
          negotiated protocol + request capabilities
                            |
                    policy evaluator
                            |
                   protocol dispatcher
                            |
          +-----------------+------------------+
          |                 |                  |
     SyncAdapter   LegacyTasksAdapter  TasksExtensionAdapter
          |                 |                  |
     attached wait      legacy methods      extension methods
```

The dispatcher selects an adapter per request; it does not construct one
deployment-wide wire mode at startup. The adapters share parameter validation,
tool descriptions, schemas, result building, and `InvocationService`. They own
their own:

- protocol version gate;
- server capability shape;
- `tools/list` execution metadata;
- `bazel.run` return type;
- task-method dispatch; and
- task status/result conversion.

This separation prevents policy evaluation from accidentally emitting a legacy
field in an extension protocol context. Golden wire tests compare the entire
capability, tool-list, creation, polling, cancellation, and result envelopes.

### `rmcp` dependency strategy

The currently pinned `rmcp 2.2.0` has typed support for the `2025-11-25` legacy
task flow. Its task types and handler hooks may be used for
`LegacyTasksAdapter` only after its emitted errors and fields pass the golden
tests.

The Tasks extension adapter requires a version of `rmcp` that implements the
accepted SEP-2663 schema, including `server/discover`, per-request capabilities,
flat polymorphic results, `tasks/update`, and ack-only cancellation.
Implementation must proceed in this order:

1. check whether a stable `rmcp` release implements the pinned extension
   revision;
2. prefer an exact upgrade when it preserves specification 002 and passes the
   complete conformance suite;
3. otherwise implement the missing wire adapter inside
   `bazel-mcp-server` using `rmcp` custom request/result facilities; and
4. do not fork protocol types into the runner, store, or domain crates.

An `rmcp` upgrade is not accepted merely because it compiles. It requires
reviewed schema diffs, all negotiated-flow transcripts, `make check`, `make test`,
`make mcp-conformance`, and the Claude Code compatibility job.

## Implementation changes by crate

### `bazel-mcp-types`

- Add `ResultDisposition`, `DeferredResultRecord`,
  `DeferredTerminalState`, `DeferredFailure`, and stable transition rules.
- Add deterministic time/expiry calculations with unit tests.
- Keep types independent of MCP names and serialization shapes.

### `bazel-mcp-store`

- Add migration 0005 and async deferred-result operations.
- Make invocation plus deferred-result creation one transaction.
- Add joined reads and cursor pagination.
- Protect minimal summaries for unexpired tasks while allowing independent raw
  evidence pruning.
- Exercise atomic commit, restart, cancellation, expiry, and pagination on the
  filesystem store.

### `bazel-mcp-runner`

- Split validation/submission from waiting.
- Give deferred invocations runner-owned cancellation tokens and worker tasks.
- Expose protocol-neutral deferred-result reads, list pages, cancellation
  recording, and expiry.
- Materialize queue/send failures without duplicating execution.
- Reconcile orphaned work to `interrupted` during startup recovery.

### `bazel-mcp-server`

- Add and validate `McpExecutionPolicy` and lifecycle settings.
- Extract the shared `RunResultBuilder` from `bazel_run`.
- Build the negotiated protocol dispatcher and three response adapters.
- Implement all wire behavior and errors defined above.
- Keep tracing and compatibility diagnostics on stderr.
- Record protocol response-byte and lifecycle metrics without logging payloads.

### `bazel-mcp-benchmark`

- Add canonical transcripts for synchronous, legacy-task, and extension-task
  execution over the same fixture.
- Count only host/model-visible task messages as model-visible output, but report
  protocol polling bytes separately.
- Verify that the final logical result and diagnostic fidelity are identical
  across negotiated flows.

No changes are required in `bazel-mcp-bep` or `bazel-mcp-reducer` beyond
possible test plumbing. Task protocol code MUST NOT enter either crate.

## Test strategy

### Unit and component tests

Configuration tests cover:

- the default `auto` policy;
- all three accepted snake-case policy values;
- unknown values;
- zero and out-of-range TTL/poll settings; and
- serialization round trips.

Domain, store, and runner tests cover:

- legal deferred-state transitions;
- atomic invocation/deferred-result creation;
- task visibility immediately after commit;
- no task row on validation failure;
- completion, Bazel failure, timeout, cancellation, and interruption;
- legacy terminal cancellation override;
- extension cancellation race outcomes;
- result survival across server restart;
- existing task handles remaining readable after an execution-policy change;
- no automatic rerun after recovery;
- expiry extension on terminal transition;
- expired-row deletion without raw-log duplication;
- legacy list ordering and pagination; and
- one child-process launch per accepted request.

### Stdio protocol conformance

Extend `scripts/test-mcp-conformance.py` or replace it with a structured
`scripts/mcp-conformance/` harness that launches fresh servers across policies
and negotiates every supported flow while recording newline-delimited JSON
transcripts.

Every transcript asserts:

- stdout contains only valid MCP JSON-RPC messages;
- stderr contains diagnostics but no secret fixture value;
- exactly three tools are listed in stable order;
- tool schemas and descriptions are identical across negotiated flows except
  for permitted legacy execution metadata;
- the selected capability shape is exact;
- the task ID equals the invocation ID;
- the task handle arrives before a deliberately delayed Bazel wrapper exits;
- `tasks/get` resolves immediately after handle creation;
- the final `CallToolResult` is byte-budgeted and logically identical to
  synchronous execution; and
- the fake Bazel wrapper records one direct argv invocation.

Required negotiated-flow cases:

| Case | Synchronous | Legacy tasks | Tasks extension |
| --- | ---: | ---: | ---: |
| successful build | yes | yes | yes |
| nonzero Bazel exit | yes | yes | yes |
| validation failure before acceptance | yes | yes | yes |
| cancellation | attached | immediate terminal task | acknowledged/raced |
| server restart after task creation | n/a | yes | yes |
| unknown/expired ID | n/a | yes | yes |
| missing task capability/opt-in under `tasks_required` | earlier protocol `-32601` | `-32601` | `-32003` |
| result retrieval method mismatch | n/a | yes | yes |
| legacy list pagination | n/a | yes | method not found |
| extension update acknowledgement | n/a | method not found | yes |

Policy coverage additionally asserts that `auto` falls back synchronously when
the request lacks task support, `sync_only` returns synchronously even for a
capable request, and `tasks_required` never starts Bazel for an incompatible
request. The harness MUST demonstrate legacy and extension dispatch under the
same server configuration in separate negotiated sessions without changing
deployment policy.

Golden fixtures normalize timestamps, UUIDs, absolute paths, and durations.
Updates require reviewed diffs and are never automatically accepted.

### Claude Code integration

Claude Code is a compatibility target for synchronous fallback and policy
enforcement. Although the MCP SDK code in Claude Code `2.1.204` recognizes the
legacy Tasks schema, the pinned host does not send `params.task`. The raw stdio
suite is therefore authoritative for the positive legacy task flow. The host
test pins and exercises the actual executable so a future behavior change is
detected instead of inferred from SDK support.

The initial compatibility lock is Claude Code `2.1.204`, the version verified
while implementing this specification. Store the accepted version and platform
checksums in `scripts/compat/claude-code.lock`. The test MUST reject a different
binary unless an explicit update workflow is used; it MUST NOT silently test a
floating `latest`.

Add:

- `make test-claude-code` for a credential-free, deterministic host integration;
- `make test-claude-code-live` for an opt-in real-provider smoke test; and
- a pinned vendor-compatibility CI job on Linux and macOS.

The credential-free test:

1. builds `bazel-mcp`;
2. creates a temporary Bazel workspace, cache, config, and non-shell test
   executable that records argv and sleeps for a known duration;
3. starts a local mock Anthropic Messages endpoint;
4. points Claude Code at it with `ANTHROPIC_BASE_URL` and a dummy test key;
5. starts Claude Code with `--bare`, `--print`, `--strict-mcp-config`, an
   isolated `--mcp-config`, no session persistence, tool permissions limited to
   the test MCP server, and nonessential network/telemetry disabled;
6. uses deterministic mock model responses that select `bazel.run` and then
   acknowledge the final tool result; and
7. places a transparent stdio recording proxy between Claude Code and
   `bazel-mcp` so assertions inspect the real host/server messages.

The mock endpoint implements and records only the Anthropic Messages API subset
observed from the pinned Claude Code version. It binds to loopback, accepts no
external connections, receives no real credentials, and returns fixed
responses. Claude Code officially supports `ANTHROPIC_BASE_URL` for gateways;
see its
[environment-variable reference](https://code.claude.com/docs/en/env-vars).
The test uses the documented
[`--mcp-config` and `--strict-mcp-config` options](https://code.claude.com/docs/en/cli-usage)
and does not patch or introspect the process.

The stdio proxy forwards bytes unchanged. Its own stdout is protocol-only, it
writes normalized traces to a temporary file, and diagnostics go to stderr.

The `sync_only` Claude case asserts:

- Claude Code discovers exactly the three MCP tools;
- `bazel.run` receives no legacy task opt-in requirement;
- the original `tools/call` stays attached until completion;
- Claude Code consumes one ordinary `CallToolResult`; and
- the wrapper ran once.

The `auto` Claude case asserts:

- Claude Code observes `bazel.run.execution.taskSupport = "optional"`;
- it does not send `tools/call.params.task`;
- the server falls back to one attached invocation; and
- Claude Code consumes the ordinary `CallToolResult`.

The `tasks_required` Claude case asserts:

- Claude Code observes `bazel.run.execution.taskSupport = "required"`;
- it still does not send `tools/call.params.task` in the pinned version;
- the server returns `-32601 Method not found` before acceptance; and
- the Bazel wrapper is not launched.

Cancellation and restart semantics remain mandatory in the raw stdio suite.
Positive legacy task execution may be added to the Claude suite when a pinned
host release actually sends the task augmentation. Cancellation may be added
when that host also exposes a stable, scriptable cancellation path; neither is
simulated through model prose.

`test-claude-code` fails clearly when the pinned binary is unavailable. It is
not silently skipped and is not part of ordinary `make test`. The dedicated CI
job installs the exact locked artifact and is a release gate. A scheduled,
non-gating job may test Claude Code `latest` to detect compatibility drift; a
version update requires reviewing the normalized protocol trace and lock file.

`test-claude-code-live` uses the same workspace and assertions with explicitly
provided credentials. It never runs in pull-request CI, never records provider
responses or secrets in committed fixtures, and has a strict budget limit.

The current Claude compatibility target does not establish host support for
either task dialect. Both positive task flows are gated by raw MCP conformance
until a production host declares and uses the relevant capability. When Claude
Code does so, the same host harness gains a positive task case.

### Make and CI integration

The Makefile adds:

```text
mcp-conformance
test-claude-code
test-claude-code-live
```

`mcp-conformance` runs all three credential-free negotiated flows and the policy
matrix. It remains separate from ordinary unit tests because it builds and
launches the production binary.

CI adds a required protocol-conformance job and a pinned Claude Code
compatibility job. The live provider test and floating-latest compatibility
test are never pull-request requirements.

Before release, run:

```text
make build
make test
make check
make mcp-conformance
make test-claude-code
make test-bazel-matrix
make fuzz-smoke
make test-token-integration
```

The long Abseil benchmark is not run as an ordinary unit test.

## Observability

Startup logs include the selected policy, task TTL, poll interval, and pinned
protocol revisions on stderr. Connection diagnostics record the negotiated
protocol family without client-provided payloads.

Add counters and histograms for:

- deferred invocations accepted, completed, failed, cancelled, interrupted, and
  expired;
- task creation acknowledgement latency;
- `tasks/get`, result, list, update, and cancellation calls by negotiated flow;
- capability mismatch and unknown/expired-ID errors;
- protocol response bytes for creation, polling, and final result separately;
- lost-handle/orphaned deferred work when detectable; and
- Claude compatibility version and pass/fail in test reports.

Telemetry never contains arguments, raw logs, diagnostic bodies, environment
values, task-result payloads, or unredacted error strings.

## Security and privacy

- UUIDv7 invocation/task IDs are generated by the receiver and unique. In the
  local stdio deployment the process boundary is the authorization boundary.
- A future remote transport MUST bind every task read, list, update, and cancel
  operation to an authenticated principal; task IDs alone are not an
  authorization model.
- Legacy `tasks/list` is available only inside the local stdio session and
  returns bounded metadata, never results or logs.
- Per-task status and immediate-response strings pass through secret redaction.
- Deferred failure text is redacted before durable commit.
- Task polling cannot request arbitrary files or bypass `bazel.inspect` policy.
- Task execution never changes argv construction: Bazel is still launched
  directly with a string vector and never through a shell.
- The Claude mock-provider test uses only a temporary allowlisted workspace and
  dummy credentials, denies non-loopback access, and disables nonessential
  network activity.

## Compatibility and rollout

As of this specification's drafting:

| Host/version | Synchronous | Legacy tasks | Tasks extension |
| --- | --- | --- | --- |
| Codex CLI 0.144.4 | compatibility target | not supported by current client result handling | no verified support |
| Claude Code 2.1.204 | pinned integration target | schema recognized, host opt-in disabled | no verified support |
| protocol harness | required | required | required |

This table is empirical, versioned test evidence rather than a permanent vendor
claim. README guidance recommends the default `auto` policy for mixed or unknown
hosts, `sync_only` as a compatibility escape hatch, and `tasks_required` only
when the deployment requires detached execution. The server, not the operator,
derives the task dialect from protocol negotiation.

Implementation is delivered in these stages:

1. Pin protocol schemas and add redacted golden transcripts for all three
   negotiated flows.
2. Add configuration, domain types, filesystem records, and retention behavior.
3. Refactor `InvocationService` into submit/wait without changing synchronous
   behavior.
4. Land legacy task negotiation and its raw protocol tests.
5. Land the pinned Claude Code `sync_only` and `tasks_required` integration job.
6. Select or adapt `rmcp` and land extension discovery plus the Tasks extension
   adapter.
7. Add benchmark transcripts, documentation, and release gates.

`auto` is the default throughout rollout. It preserves synchronous behavior for
clients that do not declare a compatible task capability, so enabling task
support does not require host-specific configuration.

## Acceptance criteria

This specification is complete when:

- `ServerConfig` accepts exactly `auto`, `sync_only`, and `tasks_required` and
  defaults to `auto`.
- The server derives legacy versus extension wire behavior from negotiated
  protocol version and request capabilities, never from host identity or
  deployment policy.
- Each negotiated protocol context emits only its specified capability, tool
  metadata, result, and task-method shapes.
- `auto` falls back synchronously for non-declaring clients, `sync_only` creates
  no new tasks, and `tasks_required` rejects incompatible calls before Bazel
  starts.
- The server still lists exactly `bazel.run`, `bazel.inspect`, and
  `bazel.cancel`.
- Legacy tasks pass the MCP `2025-11-25` golden flow, including
  `tasks/result`, list pagination, and immediate cancellation semantics.
- The Tasks extension passes the pinned SEP-2663 golden flow, including
  per-request capability enforcement, flat polymorphic creation, inlined
  terminal results, update acknowledgement, ack-only cancellation, and
  rejection of `tasks/result`.
- A task handle is never returned before its durable record is readable.
- Task and invocation IDs are identical and one accepted call launches Bazel
  once.
- Synchronous and task retrieval produce the same bounded logical result.
- Nonzero Bazel exits are completed tool results, not protocol failures.
- Task records and minimal summaries survive restart, while Bazel is never
  automatically rerun.
- Previously accepted task handles remain readable and cancellable after an
  execution-policy change until they expire.
- Task metadata adds no copy of raw logs, BEP, artifacts, or final response
  payloads.
- Expiry, cancellation races, unknown IDs, protocol mismatch errors, and restart
  recovery have deterministic tests.
- MCP stdout remains protocol-only and all diagnostic output remains on stderr.
- Redaction occurs before status text, durable metadata, and telemetry.
- The pinned Claude Code executable passes `sync_only` and `auto` synchronous
  host cases with one wrapper execution per call, and the `tasks_required`
  incompatibility case rejects before launching the wrapper.
- `make build`, `make test`, `make check`, `make mcp-conformance`,
  `make test-claude-code`, `make test-bazel-matrix`, `make fuzz-smoke`, and the
  explicit token integration target pass.
