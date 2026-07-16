# Agentic Bazel benchmark report

This is the checked-in result of the presentation benchmark recorded on
2026-07-15. It compares complete Codex coding attempts using direct shell Bazel
with otherwise equivalent attempts using `bazel-mcp`.

## Configuration

| Setting | Value |
| --- | --- |
| Provider | `codex-cli 0.144.4` |
| Model | `gpt-5.6-luna` |
| Reasoning effort | `xhigh` |
| Project | `abseil-cpp` |
| Corpus commit | `5650e9cf76d3be4318d5fa3af38ee483ddfd5e4a` |
| Bazel | `9.1.0` |
| MCP result encoding | compact JSON text |
| Samples | 5 per task and adapter |
| Run identifier | `agentic-1784163133577` |
| Adapter order | cyclic task/sample counterbalancing |

Both adapters used clean disposable snapshots of the same pinned corpus. Each
snapshot contained a protected `.bazelversion`, and all retained MCP and shell
runtime logs reported Bazel 9.1.0. The MCP arm loaded exactly the public
`bazel.run`, `bazel.inspect`, and `bazel.cancel` tools; the shell baseline did
not load MCP, so the comparison includes MCP schema overhead.

The selected tasks intentionally produce substantial Bazel output:

- a test regression that emits 512 failing cases; and
- a shared macro regression that fans out into 48 failing C++ compile actions.

Every attempt had to reproduce the failure before editing, change only the
allowed implementation or macro file, rerun focused Bazel validation, and pass
an independent hidden verifier.

## Headline results

Both adapters solved all ten attempts. `bazel-mcp` used 36.88% fewer total
tokens, with a task-clustered 95% interval of 33.47% to 39.54%.

| Adapter | Attempts | Verified | Solve rate | Input | Cached input | Uncached input | Output | Total | Tokens / solve |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| shell-default | 10 | 10 | 100.00% | 2,421,278 | 1,964,800 | 456,478 | 23,115 | 2,444,393 | 244,439 |
| bazel-mcp | 10 | 10 | 100.00% | 1,521,001 | 1,263,616 | 257,385 | 21,869 | 1,542,870 | 154,287 |

| Paired metric | Result | Task-clustered 95% interval |
| --- | ---: | ---: |
| Solve-rate delta | 0.00 pp | n/a |
| Total-token reduction | 36.88% | 33.47–39.54% |
| Uncached-input reduction | 43.62% | 35.81–49.63% |
| Concordant-solve token reduction | 36.88% | 33.47–39.54% |

Intervals use deterministic task-clustered bootstrap resampling. Concordant
metrics include only pairs where both adapters passed the independent verifier;
all ten pairs were concordant in this run.

## Task-level results

Positive reductions mean MCP used less than shell. Active tokens count
uncached input plus output, equivalent to assigning cached input zero weight.
Tool output combines command-output and MCP-result bytes visible to the model.

| Task | Pairs | Verified (shell/MCP) | Total-token reduction | Active-token reduction | Tool-output reduction | Agent-time reduction |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 48-action compile fan-out | 5 | 5/5 | 39.54% | 47.73% | -55.47% | 1.98% |
| 512-case failing test | 5 | 5/5 | 33.47% | 34.07% | 72.62% | 3.01% |

The negative fan-out tool-output result is an included agent-behavior outlier,
not Bazel output leaked by MCP. In one MCP attempt the agent ran an unbounded
repository-wide `rg -n -C 8` command, producing 471,305 bytes of source-search
output. The attempt's tool calls, bytes, time, and tokens remain in every
aggregate above.

## Cached-input sensitivity

Provider caching can change how repeated context is billed. These scenarios
count uncached input and output at full weight, then vary the weight assigned
to cached input; they are sensitivity analyses rather than pricing assumptions.

| Cached-input weight | Shell weighted tokens | MCP weighted tokens | Reduction | Task-clustered 95% interval |
| ---: | ---: | ---: | ---: | ---: |
| 0% | 479,593 | 279,254 | 41.77% | 34.07–47.73% |
| 25% | 970,793 | 595,158 | 38.69% | 33.69–42.58% |
| 100% | 2,444,393 | 1,542,870 | 36.88% | 33.47–39.54% |

The direction is unchanged across all three cache-weight scenarios.

## Agent behavior and evidence

| Adapter | Agent messages | Tool calls | Command output | MCP output | Combined tool output | Agent time |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| shell-default | 50 | 68 | 767,210 bytes | 0 bytes | 767,210 bytes | 653.2s |
| bazel-mcp | 44 | 67 | 629,473 bytes | 32,554 bytes | 662,027 bytes | 637.1s |

Despite the repository-search outlier, combined model-visible tool output was
13.71% lower with MCP. Agent time was 2.46% lower, and MCP used one fewer tool
call and six fewer agent messages across the run.

## Integrity audit

- All 20 attempts completed and all 20 patches passed their hidden verifier.
- All attempts changed exactly the intended source or macro file.
- No protected path, `.bazelrc`, or `.bazelversion` changed.
- MCP attempts made 20 successful `bazel.run` calls: one pre-fix reproduction
  and one post-fix validation per attempt.
- MCP attempts made no direct shell Bazel calls and had no failed MCP calls.
- Every retained MCP Bazel runtime log reported version 9.1.0.
- Adapter binaries were snapshotted before the first attempt, and adapter order
  was counterbalanced by task and sample.

## Interpretation and limitations

This run supports an end-to-end token-savings claim for the two tested
high-output Bazel failure classes. It does not establish that the same
percentage applies to every Bazel repository or task. The interval clusters
over only two independent task families, even though each family has five
repetitions. Provider behavior can also change independently of this repository.

See the [benchmark methodology](benchmarks.md#agentic-coding-benchmark) for the
harness design, controls, reproduction commands, and artifact layout.
