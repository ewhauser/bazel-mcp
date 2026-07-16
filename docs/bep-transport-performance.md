# BEP transport performance

Bazel MCP supports three Build Event Protocol capture paths:

- `tail`, the default, asks Bazel to write a private binary BEP file directly;
- `fifo`, an opt-in POSIX path, streams through a named pipe while mirroring
  byte-identical evidence to the private BEP file and falls back to `tail` when
  unavailable;
- `bes` starts a plaintext gRPC Build Event Service on an ephemeral loopback
  port and reconstructs the same private BEP file from ordered build events.

The default remains `tail` so Windows preview support and existing remote BES
or BuildBuddy configurations continue to work. Both optimizations are explicit:
`fifo` is POSIX-only and `bes` consumes Bazel's single `--bes_backend` slot.

## Measurement method

Two benchmarks separate transport overhead from complete Bazel invocation
latency.

The isolated benchmark replays a reviewed Bazel 9 BEP fixture 100 times per
sample. Each of nine alternating samples contains 15,800 events and 6,532,200
bytes of retained evidence. It verifies that both transports produce
byte-identical BEP files. The `tail` side writes one preassembled buffer, so it
is an intentionally aggressive bulk-file baseline rather than a simulation of
Bazel's event-by-event writer.

The live benchmark starts persistent tail, FIFO, and BES MCP servers that share
one Bazel output user root. After two warmups per mode, it rotates the order of
the modes across `bazel.run build //:bazel-mcp` calls and compares the invocation
durations reported by Bazel MCP. This includes FIFO creation, the Bazel server
PID probe, evidence spooling, reduction, and cleanup. The capture-pipeline
comparison uses nine measured samples and `--lockfile_mode=error` for both the
clean `109e183` baseline and the refactored working tree.

Results below were measured on macOS arm64 on 2026-07-16.

## Results

Tail, FIFO, and BES now feed one ordered capture pipeline. Tail frames enter
after Bazel has written them to the private evidence file. FIFO and BES frames
pass through an authoritative durable writer first; only then does the reducer
subscriber observe them. BES hands off at most 64 events or 1 MiB per bounded
batch. It flushes a partial batch after one scheduler yield, so a valid
stop-and-wait BES client cannot deadlock, and sends each protocol
acknowledgement only after the batch is accepted by the durable writer.

| Isolated capture | Median | p95 | Throughput |
| --- | ---: | ---: | ---: |
| Bulk `tail`, `109e183` baseline | 1.012 ms | 1.532 ms | 6,154 MiB/s |
| Bulk `tail`, capture pipeline | 1.078 ms | 1.515 ms | 5,781 MiB/s |
| BES, `109e183` baseline | 200.496 ms | 203.570 ms | 31.07 MiB/s |
| BES, capture pipeline | 43.953 ms | 46.033 ms | 141.73 MiB/s |

The common pipeline plus bounded handoff reduced isolated BES capture time by
78.1%, a 4.56x speedup. The bulk-tail control moved by 0.065 ms at the median
while its p95 improved slightly; that control path is unchanged and the
sub-millisecond difference is treated as run-to-run noise.

| Warm `build //:bazel-mcp` (9 samples) | Baseline median | Pipeline median | Change |
| --- | ---: | ---: | ---: |
| `tail` | 273 ms | 269 ms | -4 ms (-1.5%) |
| `fifo` | 268 ms | 269 ms | +1 ms (+0.4%) |
| `bes` | 271 ms | 272 ms | +1 ms (+0.4%) |

Whole-invocation performance is effectively flat. Tail and FIFO p95 improved
or stayed equal; BES p95 moved from 280 ms to 289 ms in this nine-sample run,
while its median changed by only 1 ms. FIFO therefore remains an explicit
POSIX optimization rather than replacing the portable default. The isolated
result shows the reduced BES ingestion cost clearly, while the live result
shows that incremental reduction and the durability gate do not materially
change ordinary build latency.

## Reproduce

Run the isolated transport benchmark:

```sh
make bench-bes-transport
```

Build the server and run the three-mode live comparison against the current
workspace:

```sh
cargo build --release -p bazel-mcp-server --bin bazel-mcp
make bench-bes-live \
  BES_LIVE_BENCHMARK_ARGS='--server target/release/bazel-mcp --samples 9 --warmups 2'
```

Both commands print a JSON report. Use
`BES_TRANSPORT_BENCHMARK_ARGS='--help'` or
`BES_LIVE_BENCHMARK_ARGS='--help'` for workload options. The live harness also
accepts repeated `--build-arg` values; for example,
`--build-arg=--lockfile_mode=error` applies the repository's strict lockfile
check to every measured invocation.
