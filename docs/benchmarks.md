# Token benchmark

The token integration benchmark measures how much Bazel-related context an
agent sees when using `bazel-mcp`, a default shell, or a shell with optimized
instructions. It also measures Bazel wall-time overhead independently from
model-visible output.

## Results

Three independent five-sample runs against a pinned Abseil commit measured an
89.73–90.69% reduction in cumulative model context compared with a default
shell agent.

The latest recorded run produced these results:

| Baseline | Context reduction | Visible-byte reduction | Median Bazel overhead |
| --- | ---: | ---: | ---: |
| Default shell | 89.73% (95% CI 87.28–91.38%) | 96.43% (95% CI 95.40–97.09%) | 0.00% |
| Optimized shell instructions | 94.46% (95% CI 91.90–95.89%) | 97.54% (95% CI 96.53–98.15%) | -0.33% |

These numbers are deterministic tokenizer estimates, not provider billing.

## Corpus and method

The corpus is
[`abseil/abseil-cpp`](https://github.com/abseil/abseil-cpp) at commit
`5650e9cf76d3be4318d5fa3af38ee483ddfd5e4a`, using Bazel 9.1.0 and
`tiktoken-rs` with the `o200k_base` encoding.

Every run covers six real build, test, query, and failure scenarios under cold
and warm cache conditions. Five samples produce 180 paired adapter
observations per run. Adapter order rotates, every observation receives an
isolated output root, and confidence intervals use deterministic paired
bootstrap resampling.

The generated reports retain environment metadata, raw evidence, checksummed
transcripts, and the inputs needed to review a comparison. Reports,
transcripts, and evidence are excluded from source control because they are
large and may contain local paths.

## Acceptance gates

The credential-free integration benchmark enforces these gates against both
shell baselines:

- at least 75% cumulative context reduction;
- at least 75% model-visible byte reduction;
- a lower 95% confidence bound of at least 75% for both reductions;
- no more than 3% median paired Bazel wall-time overhead.

## Reproduce the benchmark

Install the development dependencies described in
[CONTRIBUTING.md](../CONTRIBUTING.md), then set up the pinned corpus once:

```sh
make setup-oss-corpus
```

Run the deterministic acceptance benchmark:

```sh
make test-token-integration
```

Package the latest report and its reviewable evidence under
`.cache/published-benchmarks`:

```sh
make publish-token-benchmark
```

The scheduled
[token integration workflow](https://github.com/ewhauser/bazel-mcp/actions/workflows/token-integration.yml)
repeats the acceptance run on Linux and macOS and retains its artifact bundle
for 30 days.

## Live-agent comparison

The live benchmark uses the Codex CLI JSON event stream to compare
provider-reported input, cached-input, output, and total tokens for the same
three adapters:

```sh
make bench-token-live
```

The live comparison is separate from the deterministic acceptance gate because
provider behavior and billing metrics can change independently of the local
tokenizer estimate.

## Agentic coding benchmark

The agentic benchmark measures complete coding attempts rather than a single
Bazel invocation. Codex receives a clean, disposable snapshot of the pinned
Abseil corpus, investigates a task, edits source or BUILD files, runs Bazel as
often as needed, and returns a structured final response. A host-side verifier
then tests the patch without exposing its additional test package to the agent.
The latest presentation run is preserved in the
[checked-in agentic benchmark report](agentic-benchmark-report.md).
The compact-JSON versus TOON comparison is preserved in the
[checked-in TOON agentic benchmark report](toon-agentic-benchmark-report.md).

Every task and sample is run against identical snapshots with these adapters:

- `shell-default`: Codex uses its shell for Bazel;
- `shell-optimized`: Codex uses the shell with output and polling guidance;
- `shell-mcp-loaded`: Codex uses the shell while the Bazel MCP schemas are
  loaded but prohibited, isolating fixed MCP context overhead;
- `bazel-mcp`: compact-JSON control; Codex uses the shell for inspection and
  editing, but every Bazel invocation must use `bazel.run` or `bazel.inspect`;
- `bazel-mcp-toon`: production-default TOON encoding with the same MCP-only
  Bazel policy.

The shell adapters find an instrumented executable on `PATH` that launches
Bazel directly with an isolated output user root and preserves stdout and
stderr unchanged. For the MCP adapter, that executable refuses direct shell
Bazel calls. The harness also audits Codex JSONL events and fails tool-path
validation when the expected adapter was not used.

The harness snapshots the proxy and MCP server executables into the run
directory before starting the first attempt. Concurrent local builds therefore
cannot change adapter behavior partway through a comparison.

Codex runs with `danger-full-access` for every adapter because Bazel's
persistent server needs loopback sockets and process inspection that the macOS
`workspace-write` sandbox blocks. Keep agentic runs on the pinned disposable
corpus: this setting equalizes the comparison but is not appropriate for an
untrusted repository.

Each report includes provider-reported input, cached-input, uncached-input,
output, reasoning, and total tokens; verified solve rate; tokens per verified
solve; changed paths; patch size; agent messages; tool calls; captured command
and MCP output bytes; and end-to-end time. Task-level paired tables keep
heterogeneous workload effects visible instead of relying only on an aggregate.
Cached-input sensitivity tables
report weighted totals at 0%, 25%, and 100% cached-input weights without
assuming a particular price. Aggregate
token comparisons use paired, task-clustered bootstrap confidence intervals.
Concordant-solve savings are reported only for pairs where both adapters pass
the independent verifier. A failed or contaminated attempt therefore cannot
look efficient merely because it stopped early.

Adapter order is cyclically counterbalanced by task and sample. The checked-in
corpus currently contains four deterministic tasks:

- a source fix with a held-out whitespace regression test;
- a BUILD dependency fix whose C++ sources and tests are protected;
- a noisy 512-case test regression that requires a failing run before editing;
- a shared macro regression that fans out into 48 failing compile actions.

Set up the pinned corpus before the first run:

```sh
make setup-oss-corpus
```

Run one sample of all four tasks against the default shell and MCP adapters.
This launches eight paid Codex attempts:

```sh
make bench-agentic-smoke
```

Run the full five-sample comparison, which launches forty paid attempts for
the current four-task, two-adapter corpus:

```sh
make bench-agentic-live
```

The presentation target selects only the two high-output tasks. At five
samples it launches twenty paid attempts:

```sh
make bench-agentic-presentation
```

Run the three-arm control smoke to measure fixed MCP schema overhead. It
launches six paid attempts:

```sh
make bench-agentic-control-smoke
```

Compare the compact-JSON MCP result encoding with TOON on the two
high-output tasks. At five samples it launches twenty paid attempts:

```sh
make bench-agentic-toon
```

Agentic Make targets pin `gpt-5.6-luna` with `xhigh` reasoning by default.
Override `AGENTIC_MODEL`, `AGENTIC_REASONING_EFFORT`, or `AGENTIC_ARGS` to
change the experiment:

```sh
make bench-agentic-presentation AGENTIC_MODEL=MODEL AGENTIC_REASONING_EFFORT=high
```

Raw JSONL, stderr, verifier logs, patches, and reports are retained under
`.cache/benchmarks/abseil-cpp/agentic-<run-id>/`. They are excluded from source
control and may contain local paths. `LATEST_AGENTIC` identifies the most
recent completed run. Paid runs are manual and are not pull-request checks.
