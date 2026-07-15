# bazel-mcp

`bazel-mcp` is a local, token-efficient Model Context Protocol server for Bazel.
It runs real Bazel commands, persists the raw evidence locally, and returns a
small actionable result instead of streaming progress, repeated warnings, and
complete test logs into an LLM context.

The server exposes exactly three tools:

- `bazel.run` executes `build`, `test`, `coverage`, `query`, `cquery`, `aquery`,
  and explicitly allowed informational commands.
- `bazel.inspect` reads a bounded, filtered, paginated retained view.
- `bazel.cancel` cancels a known queued or running invocation.

## Install and configure

Build from source with Rust 1.94.1:

```sh
cargo build --release -p bazel-mcp-server
```

Copy `examples/config.toml`, set at least one `allowed_roots` entry, and register
the binary as a stdio MCP server in the host. Standard output is reserved for
MCP protocol frames; logs always go to standard error.

```json
{
  "mcpServers": {
    "bazel": {
      "command": "/absolute/path/to/bazel-mcp",
      "args": ["--config", "/absolute/path/to/config.toml"]
    }
  }
}
```

All invocation data remains in the configured local cache. `clean`, `run`, and
`shutdown` are denied by default, shell evaluation is never used, and server-
owned BEP/output-root flags cannot be overridden by a request.

Tool-result encoding is configured with `result_encoding`. The default, `text`,
returns compact JSON in one MCP text block. Set it to `toon` for a token-oriented
TOON text block, `structured` for MCP structured content, or `both` for
structured content plus backwards-compatible JSON text.

## Development

```sh
make build
make test
make check
make test-bazel-matrix
make setup-oss-corpus
make test-token-integration
```

The last command runs the credential-free, commit-pinned Abseil comparison and
enforces the 75% context/byte savings and 3% median Bazel overhead gates. See
[`specs/001-product-requirements.md`](specs/001-product-requirements.md) and
[`specs/002-project-architecture.md`](specs/002-project-architecture.md). The
configurable synchronous and task protocol shapes are specified in
[`specs/003-configurable-mcp-task-execution.md`](specs/003-configurable-mcp-task-execution.md).
