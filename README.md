# bazel-mcp

[![CI](https://github.com/ewhauser/bazel-mcp/actions/workflows/ci.yml/badge.svg)](https://github.com/ewhauser/bazel-mcp/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Run Bazel from MCP-compatible coding agents without filling their context
windows with build logs.

`bazel-mcp` is a local [Model Context Protocol](https://modelcontextprotocol.io/)
server. It runs Bazel in your workspace, returns a compact actionable result,
and keeps the complete invocation evidence available for inspection when an
agent needs more detail.

## Why bazel-mcp?

Bazel output can be enormous. Sending complete progress output, repeated
warnings, and test logs to a coding agent wastes context and can hide the error
that matters.

`bazel-mcp` gives agents:

- concise summaries of successful and failed invocations;
- structured diagnostics, test results, coverage, artifacts, and query output;
- filtered, paginated access to retained evidence;
- cancellation of queued or running commands.

Bazel still runs locally with your workspace, configured credentials,
toolchains, and remote execution settings.

## Quick start

### Requirements

- macOS or Linux
- Bazel 8 or 9, Bazelisk, or an executable workspace-local `tools/bazel`
- an MCP-compatible client
- Bazelisk when building from source (the repository pins Bazel and Rust)

### Install with Homebrew

```sh
brew install ewhauser/tap/bazel-mcp
```

### Install from a release

Download a prebuilt archive from the
[latest GitHub release](https://github.com/ewhauser/bazel-mcp/releases/latest),
or run the shell installer on macOS or Linux:

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/ewhauser/bazel-mcp/releases/latest/download/bazel-mcp-installer.sh | sh
```

### Build from source

Clone the repository and build the server with the pinned Bazel and Rust
toolchains:

```sh
git clone https://github.com/ewhauser/bazel-mcp.git
cd bazel-mcp
bazelisk build -c opt //:bazel-mcp
```

The binary is written to `bazel-bin/crates/bazel-mcp-server/bazel-mcp`.

### Connect your MCP client

Register the binary as a stdio MCP server. The exact settings location depends
on your client.

```json
{
  "mcpServers": {
    "bazel": {
      "command": "bazel-mcp"
    }
  }
}
```

If you built from source without installing the binary, use the absolute path
to `bazel-bin/crates/bazel-mcp-server/bazel-mcp` instead.

Restart the client, open a Bazel workspace, and try prompts such as:

- “Build `//app:server`.”
- “Run the tests under `//services/...` and explain any failures.”
- “Which targets depend on `//lib:core`?”

No configuration file is required. By default, the server can run Bazel in any
workspace accessible to the current user. See [Security and local data](#security-and-local-data)
to restrict it to specific roots.

## Tools

The server exposes three tools:

| Tool | Purpose |
| --- | --- |
| `bazel.run` | Run an allowed Bazel command and return a bounded summary. |
| `bazel.inspect` | Read filtered diagnostics, tests, coverage, artifacts, query results, or logs from an invocation. |
| `bazel.cancel` | Cancel a queued or running invocation. |

`bazel.run` supports `build`, `test`, `coverage`, `query`, `cquery`, `aquery`,
and selected informational commands. Successful results are limited to 2 KiB
and unsuccessful results to 8 KiB. Follow-up `bazel.inspect` calls are also
bounded and paginated, so an agent only retrieves the evidence it needs.

Failure results rank concrete root causes before aggregated action failures.
Equivalent fanout failures are represented once with `target: null` and a
`repetition_count`. Test and coverage commands use Bazel's
`--test_output=errors`. Failed-test logs are copied into private invocation
storage before they are exposed; test results report `test_log_available` or an
explicit `test_log_unavailable_reason`, never a synthetic or local failure-log
URI.

The `summary` view returns structured counts and bounded diagnostics. The `log`
and `test_log` views deliberately have the simpler logical shape below in every
result encoding:

```json
{
  "invocation_id": "019...",
  "view": "test_log",
  "items": [
    "[//foo:foo_test] assertion error: expected 3, received 4"
  ],
  "next_cursor": null,
  "truncated": false
}
```

The MCP shape has no stdout/stderr field or selector. The server automatically
normalizes, redacts, exactly deduplicates, filters, and sequences both captured
streams. Requested item limits are maxima; serialized byte packing may return
fewer items with an opaque cursor that resumes after the last emitted item.

Long calls can be returned as durable task handles when the MCP client declares
task support. With the default `auto` policy, the server discovers the
negotiated protocol and chooses synchronous execution, MCP `2025-11-25` legacy
tasks, or the `io.modelcontextprotocol/tasks` extension at runtime. This does
not add tools: task status, result, and cancellation are protocol methods.

## MCP protocol shape

MCP clients normally produce these messages on an agent's behalf, but the wire
shape is useful when integrating or debugging a host. `bazel-mcp` uses
newline-delimited JSON-RPC 2.0 over stdio; the examples below are formatted
across lines for readability and omit unrelated response fields.

| Flow | Client signal | Initial result | Final result |
| --- | --- | --- | --- |
| Synchronous | No task capability, or `sync_only` policy | `CallToolResult` | Same `tools/call` response |
| Legacy tasks | MCP `2025-11-25` plus `params.task` | Nested `result.task` | Separate `tasks/result` call |
| Tasks extension | Extension capability in request `_meta` | Flat `resultType: "task"` | Inline in terminal `tasks/get` |

<details>
<summary>Show representative JSON-RPC messages</summary>

### Synchronous call

A basic call is an ordinary MCP tool request:

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "tools/call",
  "params": {
    "name": "bazel.run",
    "arguments": {
      "workspace": "/src/project",
      "command": "build",
      "args": ["//app:server"]
    }
  }
}
```

Without a compatible task opt-in, the request remains attached and returns a
normal `CallToolResult`. The default `toon` encoding places the bounded logical
result in one text content block:

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "result": {
    "content": [{
      "type": "text",
      "text": "invocation_id: \"019f...\"\nstate: succeeded\ncommand: build\nexit_code: 0\nheadline: Build succeeded\nmore_available: true"
    }],
    "isError": false
  }
}
```

That `invocation_id` can be passed to `bazel.inspect` or `bazel.cancel`.
Different result encodings change the content representation, not the logical
result or its byte budget.

### Legacy MCP tasks

A client negotiating MCP `2025-11-25` sees
`bazel.run.execution.taskSupport = "optional"`. It opts into detached execution
by adding `params.task` to the same tool call:

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "method": "tools/call",
  "params": {
    "name": "bazel.run",
    "arguments": {
      "workspace": "/src/project",
      "command": "test",
      "args": ["//services/..."]
    },
    "task": {}
  }
}
```

The response contains a nested task. Its `taskId` is also the Bazel invocation
ID, and it is readable as soon as the handle is returned:

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "result": {
    "task": {
      "taskId": "019f...",
      "status": "working",
      "ttl": 86400000,
      "pollInterval": 2000
    },
    "_meta": {
      "io.modelcontextprotocol/related-task": {
        "taskId": "019f..."
      }
    }
  }
}
```

The client polls `tasks/get`, waits for the original `CallToolResult` with
`tasks/result`, lists durable handles with `tasks/list`, or requests
`tasks/cancel`.

### Tasks extension

Modern extension-aware clients can discover the capability with
`server/discover`:

```json
{
  "jsonrpc": "2.0",
  "id": 4,
  "method": "server/discover",
  "params": {}
}
```

```json
{
  "jsonrpc": "2.0",
  "id": 4,
  "result": {
    "capabilities": {
      "extensions": {
        "io.modelcontextprotocol/tasks": {}
      }
    }
  }
}
```

After negotiating the extension's `2026-06-30` base protocol, each participating
request declares the extension in `_meta`. The initial result is flat rather
than nested:

```json
{
  "jsonrpc": "2.0",
  "id": 5,
  "method": "tools/call",
  "params": {
    "name": "bazel.run",
    "arguments": {
      "workspace": "/src/project",
      "command": "build",
      "args": ["//app:server"]
    },
    "_meta": {
      "io.modelcontextprotocol/clientCapabilities": {
        "extensions": {
          "io.modelcontextprotocol/tasks": {}
        }
      }
    }
  }
}
```

```json
{
  "jsonrpc": "2.0",
  "id": 5,
  "result": {
    "resultType": "task",
    "taskId": "019f...",
    "status": "working",
    "ttlMs": 86400000,
    "pollIntervalMs": 2000
  }
}
```

Here the client polls `tasks/get`; once terminal, that response contains the
original `CallToolResult` inline in its `result` field. The extension has no
`tasks/result` or `tasks/list` methods. Both task dialects are durable across a
server restart, while clients that declare neither continue to use the simpler
synchronous shape.

</details>

See [specification 003](specs/003-configurable-mcp-task-execution.md) for the
complete negotiation matrix, cancellation semantics, error codes, and pinned
protocol revisions.

## Security and local data

`bazel-mcp` executes Bazel with the permissions of the user who started the MCP
client. It does not make untrusted repositories safe; use a sandbox or isolated
account for untrusted source.

The server:

- invokes Bazel directly without shell evaluation or concatenated arguments;
- denies `clean`, `fetch`, `mobile-install`, `run`, `shutdown`, and `sync` by
  default;
- prevents requests from overriding server-owned BEP and output-root flags;
- filters the child-process environment and supports configurable regex
  redaction;
- stores raw output and BEP evidence only in the local cache, using private file
  permissions.

To restrict the server to one or more workspace roots, add a repeated
`--allow-root` argument to the MCP configuration:

```json
{
  "mcpServers": {
    "bazel": {
      "command": "bazel-mcp",
      "args": ["--allow-root", "/absolute/path/to/workspaces"]
    }
  }
}
```

See [SECURITY.md](SECURITY.md) for the security policy and threat-model
guidance.

## Configuration

The built-in defaults cover personal local use. For workspace restrictions,
retention limits, timeouts, command policy, result encoding, BEP transport, or
custom redaction, start with [`examples/config.toml`](examples/config.toml).

BEP capture defaults to the private binary-file (`tail`) path so existing
remote BES and BuildBuddy configurations keep working. Set
`bep_transport = "bes"` to use bazel-mcp's loopback gRPC Build Event Service.
See [BEP transport performance](docs/bep-transport-performance.md) for the
design tradeoffs, measured results, and reproduction commands.

Pass a configuration explicitly with `--config`, set `BAZEL_MCP_CONFIG`, or
place it at `$XDG_CONFIG_HOME/bazel-mcp/config.toml` (normally
`~/.config/bazel-mcp/config.toml`). Command-line options can also add allowed
roots or override the cache directory.

See the [configuration reference](docs/configuration.md) for all settings and
their defaults.

### Result formats

TOON is the default model-visible result format. Select a different format in
the server configuration when required by an MCP host:

```toml
result_encoding = "toon"
```

| Value | Representation | When to use it |
| --- | --- | --- |
| `toon` | One token-oriented TOON text block. | Default; use for compact model context. |
| `text` | One compact JSON text block. | Use when a host or downstream tool expects JSON text. |
| `structured` | MCP `structuredContent` only. | Use with hosts verified to consume structured content without duplicating it into model context. |
| `both` | Structured content plus compact JSON text. | Compatibility mode for hosts that need both representations; it has the largest model-visible footprint. |

The setting applies to the server, not individual tool calls. Restart the MCP
server after changing it. All four choices carry the same logical result,
redaction, and byte ceilings; they only change its model-visible representation.

## Compatibility

| Component | Supported |
| --- | --- |
| Bazel | Major versions 8 and 9 |
| Platforms | macOS and Linux |
| Transport | Local MCP over stdio |
| Bazel discovery | `tools/bazel`, then Bazelisk, then Bazel on `PATH` |
| Task execution | Synchronous fallback, MCP `2025-11-25` legacy tasks, and the SEP-2663 Tasks extension |

Other Bazel major versions are rejected before an invocation is created unless
they are explicitly enabled in the configuration.

Use `mcp_execution_policy = "sync_only"` if a host cannot consume task-shaped
results. Use `tasks_required` only when detached execution is mandatory; calls
from clients without compatible capabilities are then rejected before Bazel
starts. See the [configuration reference](docs/configuration.md#configure-negotiated-task-execution)
for TTL and polling settings.

## Performance

### Deterministic invocation benchmark

Across three independent five-sample runs against a pinned Abseil corpus,
`bazel-mcp` reduced cumulative model context by 89.73–90.69% compared with a
default shell agent. The latest run measured an 89.73% reduction, with 0.00%
median paired Bazel wall-time overhead.

These results are deterministic tokenizer estimates, not provider billing. See
the [benchmark methodology](docs/benchmarks.md) for the complete results,
corpus, acceptance gates, and reproduction commands.

### Agentic coding benchmark

In a five-sample run over two high-output Abseil coding tasks,
`gpt-5.6-luna` at `xhigh` reasoning solved all 10 attempts with both the direct
shell and compact-JSON MCP adapters. The JSON MCP arm reduced
provider-reported total tokens by **36.88%** and active tokens by **41.77%**
when cached input was assigned zero weight.

| Metric | Shell Bazel | `bazel-mcp` | Reduction |
| --- | ---: | ---: | ---: |
| Verified solves | 10/10 | 10/10 | parity |
| Total tokens | 2,444,393 | 1,542,870 | 36.88% |
| Active tokens, 0% cached-input weight | 479,593 | 279,254 | 41.77% |
| Agent time | 653.2s | 637.1s | 2.46% |

A second five-sample run compared the same MCP tools using compact JSON and
TOON. Both encodings again solved every attempt, while TOON reduced total
provider tokens by **11.99%** and the retained MCP result payload by **35.69%**.

| Metric | Compact JSON | TOON | Reduction |
| --- | ---: | ---: | ---: |
| Verified solves | 10/10 | 10/10 | parity |
| Total tokens | 1,624,930 | 1,430,032 | 11.99% |
| MCP result bytes | 48,785 | 31,375 | 35.69% |

The comparison includes MCP schema and tool-call overhead. See the
[checked-in agentic benchmark report](docs/agentic-benchmark-report.md) for
task-level results, cache sensitivity, integrity checks, and limitations, or
the [TOON comparison report](docs/toon-agentic-benchmark-report.md) for the
format-specific run. See
the [agentic benchmark methodology](docs/benchmarks.md#agentic-coding-benchmark)
to reproduce it.

## Contributing

Contributions are welcome. The usual local checks are:

```sh
make build
make test
make check
```

Runner, BEP, policy, or reducer changes should also run
`make test-bazel-matrix`. Read [CONTRIBUTING.md](CONTRIBUTING.md) before opening
a pull request.

Architecture and protocol decisions are recorded under [`specs/`](specs/).

## License

`bazel-mcp` is available under the [MIT License](LICENSE).
