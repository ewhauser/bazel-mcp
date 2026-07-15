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
