# Buffa BEP spike

Status: implemented and recommended

Date: 2026-07-15

Buffa version: `0.8.1` (exactly pinned)

## Decision

Adopt Buffa for Build Event Protocol decoding. The migrated implementation
reduces the primary machine-independent work metric, retired instructions, by
55.24% for decoding and by 37.53% for the full reduction pipeline. Retaining a
large decoded stream uses 29.08% less peak resident memory. It passes the full
workspace tests, the Bazel 7/8/9 compatibility matrix, and every BEP fuzz-smoke
target.

Buffa is pre-1.0, so its workspace dependencies are pinned to `=0.8.1`. A
version update must include the same compatibility, fuzz, and benchmark gates.
The selected release is Apache-2.0 licensed. Upstream describes generated
owned/view types as its primary stability surface and reports full binary,
JSON, and text protobuf conformance; this integration uses only the binary
generated-type surface.

## Ownership model

The previous decoder materialized a complete Prost-owned object graph. Every
protobuf string, byte field, nested message, and repeated message became a
separate owned Rust value.

The new `BepEvent` model owns one immutable encoded frame through Buffa's
`BuildEventOwnedView`. The framing buffer is transferred into that handle
without copying. Reducers receive a `BuildEventView` borrowed from the handle
and use borrowed nested views throughout traversal. In particular:

- event payloads and identifiers are decoded as borrowed views;
- `NamedSetOfFiles` maps, traversal stacks, and visited sets borrow `&str`
  identifiers and file views from the retained events;
- canonical arguments, diagnostics, target results, and artifacts allocate
  only when producing the final public result;
- owned generated messages remain available for fixtures and synthetic-event
  generation, but are not used by the production decode/reduce path.

There is still one allocation per protobuf frame because the decoder consumes
an arbitrary `Read` and must retain successfully decoded frames for partial
stream recovery. The allocation becomes the view's backing storage; there is
no second protobuf object graph. The complete raw BEP remains separately on
disk as required by the retention policy.

Unknown fields are configured to be ignored, matching the product requirement
and forward-compatibility behavior of the old decoder. Frame, stream-byte, and
event-count limits and valid-prefix recovery are unchanged.

## Benchmark design

The benchmark is `bep-spike` in `bazel-mcp-benchmark`. It creates a deterministic
mixed stream with eight recurring event shapes: options, progress, named file
sets, target completion, failed actions, test results, test summaries, and
aborted events. It exercises nested messages, repeated strings and messages,
byte fields, event-id submessages, artifact graph traversal, and result
allocation.

Both implementations processed byte-identical input and produced the same
checksum:

| Workload | Events | Stream size | SHA-256 |
| --- | ---: | ---: | --- |
| Decode and full reduction | 16,000 | 3,064,474 bytes | `e0af4cc967e2c5bc11d57cbc958c2c217c7efef7c27bf908b77ef5871e29a453` |
| Retained-memory run | 200,000 | 39,027,808 bytes | `076e95d78858337305b64d1ffc2a227843fad4e9626fae6dd34846bdb61e1f09` |

The Prost baseline started from commit
`5d7792da89e05d2053a285fa2c58b2ee2ce4b6a6`; the same benchmark harness was
added before changing the decoder. The Buffa candidate was built from this
spike. Both were release builds using Rust 1.94.1 on an Apple M5 Max running
macOS 26.4.

Because unrelated work was running on the host, elapsed wall time is not used
for the decision. Each CPU workload has five samples with alternating process
order. `/usr/bin/time -lp` supplied retired instructions, CPU time, CPU cycles,
and maximum resident set size. Retired instructions are the primary comparison;
user-plus-system CPU time and cycles are corroborating measures. The table uses
sample medians.

## Results

Lower is better. Deltas are `(Buffa / Prost) - 1`.

| Workload and metric | Prost | Buffa | Delta |
| --- | ---: | ---: | ---: |
| Decode, retired instructions (400 iterations) | 65,011,554,630 | 29,101,988,579 | **-55.24%** |
| Decode, CPU seconds | 5.77 | 2.68 | **-53.55%** |
| Decode, CPU cycles | 13,711,477,687 | 6,917,323,404 | **-49.55%** |
| Decode + reducers, retired instructions (200 iterations) | 63,498,682,258 | 39,668,463,199 | **-37.53%** |
| Decode + reducers, CPU seconds | 5.87 | 3.50 | **-40.37%** |
| Decode + reducers, CPU cycles | 13,971,899,304 | 8,034,345,920 | **-42.50%** |
| Retain 200,000 decoded events, peak RSS | 280,625,152 bytes | 199,016,448 bytes | **-29.08%** |
| Release server binary | 15,907,760 bytes | 15,861,536 bytes | **-0.29%** |
| Release server `__TEXT` segment | 14,991,360 bytes | 14,942,208 bytes | **-0.33%** |

Observed wall time was highly variable: 4.23-18.47 seconds for Prost decode and
1.93-9.36 seconds for Buffa decode. That spread confirms that wall time is a
poor primary signal on this host. The independent work counters and CPU-time
medians all agree on the direction and approximate size of the improvement.

Raw retired-instruction samples:

| Workload | Prost | Buffa |
| --- | --- | --- |
| Decode | 64,923,852,434; 65,049,999,361; 64,868,740,959; 65,187,336,956; 65,011,554,630 | 29,101,988,579; 29,111,436,063; 29,052,315,563; 29,066,312,536; 29,159,891,806 |
| Decode + reducers | 63,498,682,258; 63,328,087,657; 63,252,968,010; 63,627,328,806; 63,511,986,741 | 39,729,582,527; 39,757,301,145; 39,668,463,199; 39,633,554,638; 39,633,058,648 |

Raw peak-RSS samples for the 200,000-event retention run:

| Prost | Buffa |
| --- | --- |
| 280,641,536; 280,657,920; 280,608,768; 280,625,152; 280,576,000 | 199,081,984; 199,065,600; 199,016,448; 198,967,296; 198,967,296 |

## Validation

The spike was validated with:

```text
make build
make test
make check
make test-bazel-matrix
make fuzz-smoke FUZZ_TARGET=bep_framing
make fuzz-smoke FUZZ_TARGET=bep_event_stream
make fuzz-smoke FUZZ_TARGET=named_file_sets
```

The Bazel matrix passed build success, loading failure, action failure, test
success, test failure, coverage, query, and timeout fixtures on Bazel 7.6.1,
8.4.2, and 9.1.0. The matrix uses real BEP output and complements the larger
deterministic synthetic workload used for repeatable performance measurement.

## Reproduction

Build and run the candidate benchmark with:

```sh
cargo build --release -p bazel-mcp-benchmark --bin bep-spike
/usr/bin/time -lp target/release/bep-spike \
  --mode decode --events 16000 --iterations 400 --warmup 5
/usr/bin/time -lp target/release/bep-spike \
  --mode full --events 16000 --iterations 200 --warmup 5
/usr/bin/time -lp target/release/bep-spike \
  --mode hold --events 200000 --iterations 1 --warmup 0
```

For noisy shared hosts, alternate the candidate and baseline processes and use
at least five samples. Compare retired instructions first, then CPU cycles and
user-plus-system CPU time. Do not use a single wall-time result.

## Risks and follow-up

- Buffa is pre-1.0 and may make source-breaking releases. Exact pinning and the
  generated-view compatibility tests contain that risk. Version 0.8.1 is the
  latest upstream release as of the spike date.
- Buffa is used only in `bazel-mcp-bep`; reducers consume stable re-exports from
  that crate rather than importing Buffa directly. The wrapper uses Buffa's
  public generated re-exports and does not expose generated `__buffa` modules.
- Prost remains in the final dependency graph through Turso's sync engine, but
  it is no longer a direct BEP or reducer dependency. The measured release
  server binary is nevertheless slightly smaller.
- [Upstream issue #298](https://github.com/anthropics/buffa/issues/298) reports
  an approximately 10% view-decode regression in 0.8.1 caused by a missing
  inline annotation. The results above measure the affected release, so a fix
  may improve Buffa further; it is not needed for the adoption decision.
- [Upstream issue #301](https://github.com/anthropics/buffa/issues/301) reports
  repeated-element memory amplification in owned and view decoding. Existing
  frame, stream, and event limits bound input bytes but not the number of
  repeated elements inside one frame. The current trust boundary is a local
  BEP file emitted by the Bazel process started by this server; `decode_event`
  must not be repurposed for arbitrary remote protobuf input. Track the issue
  and enable an upstream decoded-element budget when one is available.
- The open reflection/WKT conformance issue is outside this integration: the
  BEP subset uses neither Buffa reflection nor its well-known-type adapters.
- If profiling later shows per-frame allocation overhead to be significant, a
  bounded shared-buffer decoder can retain Buffa byte slices from larger input
  slabs. That is independent of this ownership migration and must preserve
  partial-stream recovery and stream limits.
