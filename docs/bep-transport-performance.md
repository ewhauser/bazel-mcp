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
PID probe, evidence spooling, reduction, and cleanup.

Results below were measured on macOS arm64 on 2026-07-16.

## Results

The first BES implementation performed two asynchronous file writes for each
event: one for the varint length and one for the payload. The optimized pass
reuses a framing buffer and writes each complete frame once before sending its
acknowledgement.

| Isolated capture | Median | p95 | Throughput |
| --- | ---: | ---: | ---: |
| Bulk `tail` baseline, final run | 1.17 ms | 1.58 ms | 5,342 MiB/s |
| BES, first pass | 389.85 ms | 394.95 ms | 15.98 MiB/s |
| BES, optimized | 203.59 ms | 211.99 ms | 30.60 MiB/s |

The second pass reduced isolated BES capture time by 47.8%. Its remaining cost
is primarily per-event gRPC decoding, validation, file-write submission, and
acknowledgement. Dividing the optimized median by the event count gives about
12.9 microseconds per event.

| Warm `build //:bazel-mcp` (7 samples) | Median | p95 |
| --- | ---: | ---: |
| `tail` | 275 ms | 277 ms |
| `fifo` | 271 ms | 273 ms |
| `bes` | 275 ms | 282 ms |

FIFO was 4 ms (1.5%) faster than regular-file tailing at the median on this
workload, while BES matched the tail median. The FIFO improvement is modest,
so FIFO remains an explicit POSIX optimization rather than replacing the
portable default. The isolated bulk-write comparison still makes BES's
per-event protocol overhead visible even though it overlaps with the build in
the live workload.

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
