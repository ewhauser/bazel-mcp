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

## Install

Build from source with Rust 1.94.1:

```sh
cargo build --release -p bazel-mcp-server
```

Register the binary as a stdio MCP server in the host. No configuration file is
required. Standard output is reserved for MCP protocol frames; logs always go
to standard error.

```json
{
  "mcpServers": {
    "bazel": {
      "command": "/absolute/path/to/bazel-mcp"
    }
  }
}
```

Built-in defaults allow any Bazel workspace while retaining command,
environment, timeout, storage, and output limits. To restrict workspace paths or
customize other settings, copy `examples/config.toml` and pass it with
`--config`, set `BAZEL_MCP_CONFIG`, or place it in the OS user configuration
directory.

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
[`specs/002-project-architecture.md`](specs/002-project-architecture.md).
