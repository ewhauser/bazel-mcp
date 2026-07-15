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
- Bazel 7, 8, or 9, Bazelisk, or an executable workspace-local `tools/bazel`
- an MCP-compatible client
- Rust 1.94.1 when building from source

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

Clone the repository and build the server with the pinned Rust toolchain:

```sh
git clone https://github.com/ewhauser/bazel-mcp.git
cd bazel-mcp
cargo build --release -p bazel-mcp-server
```

The binary is written to `target/release/bazel-mcp`.

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
to `target/release/bazel-mcp` instead.

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
retention limits, timeouts, command policy, result encoding, or custom
redaction, start with [`examples/config.toml`](examples/config.toml).

Pass a configuration explicitly with `--config`, set `BAZEL_MCP_CONFIG`, or
place it at `$XDG_CONFIG_HOME/bazel-mcp/config.toml` (normally
`~/.config/bazel-mcp/config.toml`). Command-line options can also add allowed
roots or override the cache directory.

See the [configuration reference](docs/configuration.md) for all settings and
their defaults.

## Compatibility

| Component | Supported |
| --- | --- |
| Bazel | Major versions 7, 8, and 9 |
| Platforms | macOS and Linux |
| Transport | Local MCP over stdio |
| Bazel discovery | `tools/bazel`, then Bazelisk, then Bazel on `PATH` |

Other Bazel major versions are rejected before an invocation is created unless
they are explicitly enabled in the configuration.

## Performance

Across three independent five-sample runs against a pinned Abseil corpus,
`bazel-mcp` reduced cumulative model context by 89.73–90.69% compared with a
default shell agent. The latest run measured an 89.73% reduction, with 0.00%
median paired Bazel wall-time overhead.

These results are deterministic tokenizer estimates, not provider billing. See
the [benchmark methodology](docs/benchmarks.md) for the complete results,
corpus, acceptance gates, and reproduction commands.

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
