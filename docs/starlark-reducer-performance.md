# Starlark reducer performance

This report compares the custom-reducer extension contract implemented directly
in Rust with the same regular-expression diagnostic extraction invoked from
Starlark.

## Result

The Starlark adapter took 88.823 microseconds for one error, 1.1772 milliseconds
for 100 errors, and 10.718 milliseconds for 1,000 errors. Native Rust was
134.74x, 27.90x, and 25.06x faster respectively.

| Error lines | Input bytes | Native median | Starlark median | Starlark/native |
| ---: | ---: | ---: | ---: | ---: |
| 1 | 40 | 659.21 ns | 88.823 us | 134.74x |
| 100 | 4,371 | 42.194 us | 1.1772 ms | 27.90x |
| 1,000 | 46,672 | 427.76 us | 10.718 ms | 25.06x |

Criterion's 95% confidence intervals were:

| Error lines | Native interval | Starlark interval |
| ---: | ---: | ---: |
| 1 | 654.83-663.69 ns | 87.866-89.861 us |
| 100 | 42.091-42.316 us | 1.1626-1.1969 ms |
| 1,000 | 426.05-429.60 us | 10.687-10.749 ms |

The one-line case exposes the adapter's fixed costs: serializing the bounded
context, creating an evaluator heap, invoking the frozen function, compiling
the host regular expression, converting the returned Starlark value, and
validating the typed patch. At larger inputs, diagnostic construction dominates
and the ratio settles near 25x. At 1,000 errors, native throughput was 104.05
MiB/s and Starlark throughput was 4.153 MiB/s.

The absolute cost is post-Bazel processing and remains below 11 ms for the
largest case measured. That makes Starlark reasonable for explicitly configured
rule-specific reducers, while the built-in high-frequency reducers should stay
in Rust. Multiple matching Starlark reducers add their evaluation costs, so
selectors should be narrow. Caching declared regular expressions at module load
is the clearest future optimization if profiles show custom reduction becoming
material.

## Method

The benchmark constructs deterministic compiler errors in the form
`path:line:column: error: message`. Both arms use Rust's `regex` crate with the
same pattern and emit the same typed `Diagnostic` fields. The native arm uses a
precompiled expression. The Starlark arm calls the supported
`regex_diagnostics` host function and therefore includes the production adapter
path: context conversion, Starlark evaluation, expression compilation, output
conversion, and exact schema validation.

The 1,000-line case intentionally measures all 1,000 produced diagnostics even
though the ordinary model-visible result budget retains fewer. This is a
worst-case extension cost before the shared final budget.

The benchmark excludes server startup and module compilation, which happen
once, as well as Bazel execution, BEP decoding, storage, redaction, common
diagnostic finalization, and MCP serialization. Run it with:

```sh
make bench-reducers
```

Source: [`custom_reducers.rs`](../crates/bazel-mcp-benchmark/benches/custom_reducers.rs).

## Environment

- Date: 2026-07-16
- Platform: macOS 15.7.7 (24G720), arm64
- Rust: 1.97.0 (Homebrew), LLVM 22.1.8
- Profile: Cargo `bench`, optimized
- Criterion: 0.7.0, 100 samples per case

These are local microbenchmarks, not cross-machine service-level objectives.
Re-run them on deployment hardware before using the absolute values for
capacity planning.
