# Source-agnostic diagnostic reduction

The reducer is split into four layers with a one-way dependency boundary:

```text
diagnostic-reducer-cli
        |
diagnostic-reducer        bazel-mcp-reducer
        |                         |
        +---- diagnostic-reducer-core
```

- `diagnostic-reducer-core` owns streaming scopes, normalized line framing,
  parser lifecycle, provenance, path mapping, redaction, deduplication, ranking,
  and bounded output. It contains no parsing grammar.
- `diagnostic-reducer` supplies the immutable built-in compiler, linter,
  runtime, and test-log parser plan plus the original synchronous batch API.
- `diagnostic-reducer-cli` incrementally reads stdin or files and renders human,
  JSON, JSONL, SARIF, or GitHub workflow annotations.
- `bazel-mcp-reducer` owns Bazel path compaction, Starlark/Aspect/rules_go
  semantics, Bazel status fallbacks, BEP arbitration, and Bazel category mapping.

The three generic packages are explicitly versioned and publishable. They use
only public package dependencies so they can be lifted into a separate GitHub
repository without moving any Bazel, MCP, storage, or runner code.

## Streaming contract

A caller constructs an ordered `ParserPlan`, starts a caller-owned `Scope`, and
pushes arbitrary byte chunks for named streams. The core normalizes terminal
control sequences, handles CR/LF/CRLF across chunk boundaries, and emits lines
with stable scope, stream, line, and parser provenance. Ending a scope reports
whether its evidence was complete, truncated, cancelled, or interrupted.

Hard limits cover open scopes, retained bytes per scope, line length, parser
candidates, returned items, and serialized finding bytes. Exceeding a bound is
reported through counters and `truncated`; it never causes unbounded retention.
Structured CI annotations can enter through `emit_structured` and pass through
the same transformation and budgeting path as parsed text.

## Output policy

Finalization always applies this order:

1. caller-supplied path mapping;
2. caller-supplied redaction and control sanitization;
3. fallback-overlap suppression;
4. exact deduplication with repetition counts;
5. typed ranking by severity, class, evidence quality, and stable provenance;
6. item and serialized-byte budgets.

Raw evidence remains the caller's responsibility. The core performs no
persistence and returns only sanitized findings and text-free accounting.

## Compatibility and verification

The batch `reduce` entry point remains available and uses the same parser and
finalization behavior. Streaming-versus-batch tests replay representative Rust,
Python, Go, protobuf, and TypeScript logs at every chunk width through 64 bytes.
The test-log accumulator also has chunk-invariance and incomplete-scope tests.

The Bazel adapter is protected by the existing Bazel 8/9 golden suites and the
reducer integration corpus. Generic no-match and mixed-tail benchmarks cover
batch input and 64 KiB/1 KiB streaming chunks. The fuzz target feeds arbitrary
terminal bytes through both APIs with arbitrary chunk boundaries and bounds.

Use `make check-diagnostic-reducer-boundary` for the dependency/semantic audit,
`make bench-generic-reducers` for the streaming benchmark, and
`diagnostic-reduce --help` for CLI options.
