# Reducer case harness

This crate discovers the versioned contracts under
`testdata/reducer-corpus`, replays sanitized evidence through the production
reducers, and runs declared examples through `bazel-mcp` for live verification.

```sh
cargo run -p bazel-mcp-reducer-cases -- list
cargo run -p bazel-mcp-reducer-cases -- verify
cargo run -p bazel-mcp-reducer-cases -- verify --live --tag fast
cargo run -p bazel-mcp-reducer-cases -- record --case go/test/failure
BAZEL_MCP_ACCEPT_REDUCER_CASES=1 \
  cargo run -p bazel-mcp-reducer-cases -- accept --case go/test/failure --yes
```

`record` writes only `actual.*`; `accept` is deliberately gated and revalidates
the recording before replacing checked-in evidence. See
[`docs/reducer-integration-testing.md`](../../docs/reducer-integration-testing.md)
for the contract, sanitization rules, and contribution workflow.
