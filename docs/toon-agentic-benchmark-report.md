# TOON agentic benchmark report

This is the checked-in result of the TOON comparison recorded on 2026-07-16.
It compares complete Codex coding attempts using the then-default compact-JSON
MCP result encoding with otherwise equivalent attempts using TOON. TOON became
the production default after this run.

## Configuration

| Setting | Value |
| --- | --- |
| Provider | `codex-cli 0.144.4` |
| Model | `gpt-5.6-luna` |
| Reasoning effort | `xhigh` |
| Project | `abseil-cpp` |
| Corpus commit | `5650e9cf76d3be4318d5fa3af38ee483ddfd5e4a` |
| Bazel | `9.1.0` |
| Samples | 5 per task and encoding |
| Run identifier | `agentic-1784182634748` |
| Adapter order | cyclic task/sample counterbalancing |

Both arms loaded exactly `bazel.run`, `bazel.inspect`, and `bazel.cancel`, used
the same MCP-only Bazel policy and agent prompt, and ran against clean snapshots
of the pinned corpus. Apart from their isolated per-attempt paths, their server
configurations differed only in `result_encoding`: `text` for the default arm
and `toon` for the candidate.

## Headline results

Both encodings solved all ten attempts. TOON used 11.99% fewer total tokens,
with a task-clustered 95% interval of 8.00% to 15.29%.

| Encoding | Attempts | Verified | Solve rate | Input | Cached input | Uncached input | Output | Total | Tokens / solve |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Default JSON text | 10 | 10 | 100.00% | 1,599,017 | 1,264,640 | 334,377 | 25,913 | 1,624,930 | 162,493 |
| TOON text | 10 | 10 | 100.00% | 1,405,163 | 1,155,072 | 250,091 | 24,869 | 1,430,032 | 143,003 |

| Paired metric | Result | Task-clustered 95% interval |
| --- | ---: | ---: |
| Solve-rate delta | 0.00 pp | n/a |
| Total-token reduction | 11.99% | 8.00–15.29% |
| Uncached-input reduction | 25.21% | 0.65–40.03% |
| Concordant-solve token reduction | 11.99% | 8.00–15.29% |

TOON reduced the retained MCP result payloads from 48,785 to 31,375 bytes, a
35.69% reduction. Provider token savings are smaller because result payloads
are only part of the complete multi-turn agent context.

## Task-level results

Positive reductions mean TOON used less than default. Active tokens count
uncached input plus output. Tool output combines command and MCP-result bytes.

| Task | Pairs | Verified (default/TOON) | Total-token reduction | Active-token reduction | Tool-output reduction | Agent-time reduction |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 48-action compile fan-out | 5 | 5/5 | 15.29% | 38.04% | 85.61% | 4.14% |
| 512-case failing test | 5 | 5/5 | 8.00% | 0.62% | 2.53% | 1.07% |

TOON used fewer total tokens in eight of the ten matched pairs. Both task
families had positive aggregate reductions.

## Cached-input sensitivity

These scenarios count uncached input and output at full weight, then vary the
weight assigned to cached input. They are sensitivity analyses rather than
pricing assumptions.

| Cached-input weight | Default weighted tokens | TOON weighted tokens | Reduction | Task-clustered 95% interval |
| ---: | ---: | ---: | ---: | ---: |
| 0% | 360,290 | 274,960 | 23.68% | 0.62–38.04% |
| 25% | 676,450 | 563,728 | 16.66% | 5.33–25.02% |
| 100% | 1,624,930 | 1,430,032 | 11.99% | 8.00–15.29% |

The direction is unchanged across all three cache-weight scenarios.

## Agent behavior and evidence

| Encoding | Agent messages | Tool calls | Command output | MCP output | Agent time |
| --- | ---: | ---: | ---: | ---: | ---: |
| Default JSON text | 46 | 66 | 569,173 bytes | 48,785 bytes | 732.6s |
| TOON text | 45 | 70 | 93,257 bytes | 31,375 bytes | 713.0s |

One default fan-out attempt ran an unbounded repository-wide source search that
produced 472,157 bytes of command output. The attempt remains in every canonical
aggregate above. As a non-canonical sensitivity check, dropping that entire
matched pair leaves 9/9 solves per arm, a 10.96% total-token reduction, and a
12.40% active-token reduction. This supports the total-token direction while
showing that the headline active-token result is sensitive to agent behavior
outside MCP result encoding.

## Integrity audit

- All 20 attempts completed and passed the independent hidden verifier.
- All attempts changed exactly the intended implementation or macro file.
- No protected path or `.bazelversion` changed.
- Each arm made 20 successful `bazel.run` calls: one pre-fix reproduction and
  one post-fix validation per attempt.
- The default arm made five `bazel.inspect` calls and the TOON arm made three;
  none of the 48 completed MCP calls had a protocol error.
- Neither arm invoked Bazel through the shell.
- Ten retained server configurations used `text`; ten used `toon`.
- Adapter binaries were snapshotted before the first attempt, and adapter order
  was counterbalanced by task and sample.

## Interpretation and limitations

This run supports TOON for the tested high-output Bazel failure classes: solve
rate was unchanged, direct MCP output was smaller, and total provider tokens
fell under every cache-weight scenario. It does not establish the same effect
for low-output results, deeply nested or irregular responses, other models, or
other repositories. The confidence interval clusters over two independent task
families, and provider behavior can change independently of this repository.

See the [benchmark methodology](benchmarks.md#agentic-coding-benchmark) for the
harness controls, reproduction commands, and artifact layout.
