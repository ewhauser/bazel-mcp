# Reducer integration examples

These self-contained Bazel workspaces produce real compiler, linker, test, and
Bazel failures used to validate `bazel-mcp` reducers. They are both executable
documentation and the source of the sanitized recordings under
`testdata/reducer-corpus`.

Each workspace pins its own Bzlmod dependencies. Successful targets demonstrate
normal use; targets tagged `manual` and `reducer-fixture` intentionally fail and
are invoked explicitly by the `reducer-cases` harness through `bazel.run`.

```bash
cargo build -p bazel-mcp-server --bin bazel-mcp
cargo run -p bazel-mcp-reducer-cases -- list
cargo run -p bazel-mcp-reducer-cases -- verify
cargo run -p bazel-mcp-reducer-cases -- verify --live --tag fast
```

Do not invoke failing examples directly when recording reducer evidence. The
harness owns isolated Bazel state, MCP invocation, sanitization, provenance,
semantic verification, and explicit golden acceptance.

| Workspace | Live failure families |
| --- | --- |
| `bazel-core` | loading, visibility, analysis, action failure and fanout |
| `cpp` | compiler, missing header, linker, gTest assertion and exception |
| `go` | compiler and `go test` |
| `python` | syntax, import, and traceback |
| `starlark` | syntax, loading, macro, and rule analysis |
| `jvm` | javac symbol and JVM assertion |
| `protobuf` | syntax, missing import, and undefined message |
| `node` | TypeScript compiler, JavaScript syntax, and Node runtime |
| `rust` | libtest assertion and panic |

See [`docs/reducer-integration-testing.md`](../docs/reducer-integration-testing.md)
for the contribution workflow.
