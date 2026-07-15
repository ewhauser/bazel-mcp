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

Bazel 7, 8, and 9 are accepted and exercised in the integration matrix.
Untested major versions fail before an invocation is created unless
`allow_unsupported_bazel_versions` is explicitly enabled. Workspace-local
`tools/bazel` wrappers and Bazelisk are supported.

## Benchmarks

Three independent five-sample runs against a pinned Abseil commit show an
89.73–90.69% reduction in cumulative model context versus a default shell
agent. The latest run measured 89.73% (95% CI 87.28–91.38%). It reduced
model-visible bytes by 96.43% (95% CI 95.40–97.09%) while median paired Bazel
wall-time overhead was 0.00%.

| Baseline in latest run | Context reduction | Visible-byte reduction | Median Bazel overhead |
| --- | ---: | ---: | ---: |
| Default shell | 89.73% (87.28–91.38%) | 96.43% (95.40–97.09%) | 0.00% |
| Optimized shell instructions | 94.46% (91.90–95.89%) | 97.54% (96.53–98.15%) | -0.33% |

The corpus is Abseil at commit
`5650e9cf76d3be4318d5fa3af38ee483ddfd5e4a`, using Bazel 9.1.0 and
`tiktoken-rs` with `o200k_base`. Every run covers six real build, test, query,
and failure scenarios under cold and warm cache conditions: 180 paired adapter
observations per run. Adapter order rotates, every observation gets an isolated
output root, and confidence intervals use deterministic paired bootstrap
resampling. The reports retain environment metadata, raw evidence, and
checksummed transcripts.

These are deterministic tokenizer estimates, not provider billing. The
scheduled token workflow repeats the acceptance run on Linux and macOS and
uploads the same reviewable artifact format for 30 days. Benchmark reports,
transcripts, and evidence are intentionally excluded from source control. Run
`make publish-token-benchmark` to package the latest local run under
`.cache/published-benchmarks`, or download a bundle from the
[token integration workflow](https://github.com/ewhauser/bazel-mcp/actions/workflows/token-integration.yml).
`make bench-token-live` uses the Codex CLI JSON event stream to compare
provider-reported input, cached-input, output, and total tokens for the same
three adapters.

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
enforces the 75% context/byte savings (including the lower 95% confidence
bound) and 3% median Bazel overhead gates against both shell baselines. See
[`specs/001-product-requirements.md`](specs/001-product-requirements.md) and
[`specs/002-project-architecture.md`](specs/002-project-architecture.md). The
configurable synchronous and task protocol shapes are specified in
[`specs/003-configurable-mcp-task-execution.md`](specs/003-configurable-mcp-task-execution.md).
