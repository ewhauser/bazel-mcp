# Bazel MCP ledger feature and performance report

## Scope and provenance

Work started on `codex/ledger-performance` from fetched `origin/main` commit
`a439d44345113a63e63a7b00c330eb20a48538f6`.

Homebrew initially provided `ewhauser/tap/bazel-mcp-server` 0.1.0. Its release
archive checksum was
`ea0bf3799c369f5cf542c34eeeff107aa3c136c494a51213ffbdc4102a69de07`, and the
installed executable checksum was
`5fd34748f3d6dc6a0ecad394a3a497ebd52711646fe757b344909839c41780f1`.
That executable was preserved only as provenance because it was older than
`origin/main` and was not used as the performance baseline.

The fetched `origin/main` source was built through `bazel.run` for
`//:bazel-mcp` (invocation `019f6994-4fee-7f20-aadc-fb8b7968336a`). The resulting
0.2.0 executable was copied over the Homebrew-linked executable and preserved
as the benchmark baseline with SHA-256
`13aefacd618b00aafefc056a7e52d870df6752a9b96350b5a476bbe1b0b069c3`.

The first optimized candidate at `c40715f` was built through `bazel.run` and
preserved with SHA-256
`1d68388fef79834e58c9cb043d07ef6d3a020dfc78e9ed4d84621f52c6408f0a`.
The exact deliverable was rebuilt through `bazel.run` after the test-only lint
cleanup and validation fixes (invocation
`019f69be-420d-7cc1-afdd-3fcb8116be94`), copied over the Homebrew-linked
executable, and preserved with SHA-256
`e0612520f06a37b62298d377d1c08a6d360fb0b5efc80041dbd1f1ea9d0930a0`.

## Sequential features

1. `6626127` exposes a workspace-scoped retained-invocation ledger through
   `bazel.inspect` with `view=invocations`.
2. `7695d8a` adds a caller-selected inspection response ceiling clamped from 512
   through 8192 model-visible bytes.
3. `e9c6efd` adds invocation-state filtering and binds pagination cursors to the
   state filter.
4. `7b910aa` adds Bazel-command filtering and binds pagination cursors to the
   command filter.
5. `9f61b81` adds a dedicated `metrics` view for lifecycle timestamps,
   termination, byte counts, queue/Bazel/reduction latency, and inspection
   telemetry.

Each feature was validated before the next feature began, and each completed
feature was rebuilt and copied over the Homebrew-linked executable.

## Deliberately exercised failures

| Failure | Invocation | Evidence and recovery |
| --- | --- | --- |
| Rust compiler | `019f6997-c24d-7a83-9b75-19745e5a10bd` | Making `invocation_id` optional before updating its consumer produced E0308: expected `&str`, found `&Option<String>`. `bazel.inspect view=log` retained the exact location and type mismatch. Updating the handler and adding a ledger test restored the build. |
| Rust test | `019f699a-896e-7581-b163-d5a1ba6e9b5a` | A stale budget expectation produced Bazel test exit code 3. `view=test_log` identified `inspection_byte_budget_is_bounded`, with left 8192 and right 4096. Correcting the expected default restored the test. |
| BUILD file | `019f69a0-94b5-7c10-9dcb-e44376af68ef` | The dependency label `bazel_mcp_runer` failed during analysis. The initial MCP result retained Bazel's `did you mean bazel_mcp_runner?` suggestion. Correcting the BUILD label restored analysis and tests. |

## Ledger review and generic improvements

The first ledger implementation returned complete durable records. Three rows
filled the 8 KiB ceiling because every row repeated canonical startup flags,
full summaries, diagnostics, and local cache paths. At 4 KiB the result could
not fit at all. With multiple failed rows, generic string shrinking eventually
replaced IDs, states, commands, and diagnostics with empty strings, satisfying
the byte limit while destroying the evidence.

`59df058` replaces those records with purpose-built rows containing identity,
workspace, state, command, bounded requested arguments, timestamps, exit code,
duration, headline, target/test counts, and selected byte telemetry. It also
retries with a smaller page size so the returned opaque cursor follows the last
emitted row. The same 4 KiB failed-only request then returned all three failures
with intact evidence.

The ledger also showed zero model-visible bytes and inspection calls for
short-lived MCP clients. The store intentionally debounced telemetry writes by
250 ms, but stdio shutdown could drop the pending update. `199ad3a` adds an
explicit pending-telemetry flush during graceful shutdown. A fresh `info`
invocation subsequently persisted 493 model-visible bytes; after one separate
metrics inspection, the next server process observed 986 bytes and one inspect
call.

Finally, profiling the benchmarked handler path showed that JSON results were
serialized once for byte accounting and again for response encoding. `c40715f`
combines those operations without changing text, TOON, structured, or combined
result semantics.

## Benchmark method and results

[`run-mcp-inspect-latency.py`](../scripts/benchmarks/run-mcp-inspect-latency.py)
starts each preserved executable with a fresh store outside the workspace,
initializes MCP, creates one retained record through `bazel.run`, warms the
selected `bazel.inspect` view, and measures repeated stdio tool-call round
trips. Server startup and the setup `bazel.run` are deliberately outside the
sample. Baseline/candidate order alternates by pair. The environment was macOS
15.7.7 arm64, Python 3.14.6, and Bazel 9.2.0.

| Workload | Samples | Baseline median | Candidate median | Median ratio improvement | Median paired improvement | Minimum paired improvement |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `info release`, `summary` | 9 pairs, 1,000 calls each, 100 warmups | 156.319 us | 145.788 us | 6.74% | 6.72% | 4.23% |
| `query //...`, `query_results`, limit 20 | 7 pairs, 500 calls each, 50 warmups | 331.095 us | 302.434 us | 8.66% | 8.71% | 7.19% |

Example reproduction, after producing preserved baseline and candidate binaries
through `bazel.run`:

```sh
python3 scripts/benchmarks/run-mcp-inspect-latency.py \
  --baseline /path/to/origin-main-bazel-mcp \
  --candidate /path/to/candidate-bazel-mcp \
  --baseline-label origin-main-a439d44 \
  --candidate-label final \
  --workspace "$PWD" \
  --cache-parent "$HOME/Library/Caches/bazel-mcp-inspect-benchmark" \
  --pairs 9 --calls 1000 --warmup 100
```

The query cross-check adds `--command query --argument //... --view
query_results --limit 20 --pairs 7 --calls 500 --warmup 50`.

## Validation

- `make test` passed the complete Cargo workspace, including 16 runner process
  tests, 30 store tests, server shutdown coverage, reducer goldens, and doc
  tests.
- `make mcp-conformance` passed synchronous, legacy-task, extension-task, and
  policy conformance while retaining the three-tool public surface.
- `cargo fmt --all -- --check`, Clippy across the workspace with all targets and
  features under `-D warnings`, and `cargo shear` all passed. `make check` could
  not enter its Nix development shell because this host has no `nix` command,
  so those underlying checks were run directly.
- `bazel.run test //...` passed 26 targets and all 12 Bazel tests in invocation
  `019f69bb-6554-75e2-82a8-8370bd0cfc2a`. Its first clean run exposed a missing
  direct BEP dependency in the standalone runner process test; `c49b356` fixed
  the BUILD declaration before the successful rerun.
- The direct Bazel matrix passed all 14 cases on Bazel 7.6.1, 8.4.2, and 9.1.0.
  A focused 9.1.0 rerun passed after `598cea3` moved generated workspaces outside
  the repository so later `//...` traversals cannot enter them.
- Python bytecode compilation for the benchmark harness and shell syntax
  validation for the matrix script passed.
- `make fuzz-smoke` could not start because the host has neither `rustup` nor a
  nightly Cargo toolchain. No fuzz result is claimed.

## Overfitting risks

- These are local model-boundary microbenchmarks, not end-to-end Bazel build
  benchmarks. They intentionally exclude server startup and Bazel execution.
- The result is specific to macOS arm64, local stdio, the text encoding, one
  repository, and two inspection shapes. TOON, structured output, Windows,
  Linux, remote filesystems, and high concurrency may have different ratios.
- Summary timings had scheduler noise: one of nine paired rounds improved by
  less than 5%, even though both median measures exceeded the gate. The claim is
  therefore a median improvement, not a per-call or per-run guarantee.
- The query-results cross-check reduces single-payload overfitting risk and all
  seven exact-deliverable pairs exceeded 7%, but it still uses warm local
  caches.
- Candidate functionality differs from the baseline because the five requested
  features and ledger fixes are present. The optimized code path itself is
  generic across every tool result, but future regressions should be assessed
  against the preserved checksums and multiple response sizes.
