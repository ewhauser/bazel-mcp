# Storage design and performance

## Decision

Bazel MCP does not need a database for short-lived invocation evidence. The
production store uses private, database-free invocation directories. Bazel
writes raw evidence directly to disk while it runs; the service retains compact
indexes and reducer state in memory, but never duplicates complete stdout,
stderr, or BEP streams in application buffers.

The canonical per-invocation layout is:

```text
invocations/<uuidv7-day>/<16-way-shard>/<invocation-id>/
  manifest.json    redacted request + lifecycle + compact summary + accounting
  details.json     detailed targets, tests, and per-file coverage
  artifacts.json   artifact array
  stdout.log       raw bytes written directly by Bazel
  stderr.log       raw bytes written directly by Bazel
  events.bep       Bazel varint-delimited protobuf frames
```

The request exists only in `manifest.json`. The summary header exists only in
`manifest.json`; large collections exist only in their sidecars. Empty detail
and artifact sidecars are omitted. There is no migration or legacy-layout
reader because no released installation uses the previous layout.

## Logical data flow and formats

```text
1. MCP request
   input:  MCP JSON object for bazel.run
   memory: typed InvocationRequest
      |
      | validate argv/workspace/environment; redact durable fields
      v
2. Acceptance commit
   disk:   manifest.json (versioned compact JSON)
   method: private manifest.tmp -> atomic rename
   index:  compact IndexEntry under RwLock<Index>
      |
      | spawn Bazel directly with an argv vector and file handles
      v
3. Bazel execution
   disk:   stdout.log / stderr.log (raw bytes, direct child output)
           events.bep (varint-length-delimited protobuf, direct Bazel output)
   memory: only process state, cancellation handle, progress counters
      |
      | process exits, is cancelled, or times out
      v
4. Bounded reduction
   reads:  events.bep frame by frame into BepAccumulator
           stdout.log query rows in 1 MiB byte chunks for newline counting
           returned query rows only, capped at 64 KiB retained / 4 KiB visible
           2 MiB log tails with complete BEP, otherwise 8 MiB tails
   memory: bounded reducer state + bounded log tails; no whole raw stream
      |
      | redact summaries, canonical argv, artifacts, filters, and output
      v
5. Terminal commit
   disk:   details.json (compact JSON object of arrays, if nonempty)
           artifacts.json (compact JSON array, if nonempty)
           manifest.json (one coalesced atomic JSON replacement)
   fields: state + termination + summary + final metrics + canonical argv
           + payload byte accounting + deferred-result expiry
      |
      v
6. bazel.inspect read
   lookup: RwLock<Index> point lookup, then per-invocation read lock/serialization
   reads:  manifest-backed compact record, selected JSON sidecar, or a bounded
           byte range from a raw log/query file
   cursor: versioned fixed binary payload, URL-safe base64 without padding
      |
      | redact before filtering/counting whenever a filter is present
      v
7. MCP result
   output: existing JSON, TOON, or structured MCP representation
   limits: serialized model-visible byte budget, opaque continuation cursor
```

`details.json` and `artifacts.json` use compact JSON rather than NDJSON or
framed protobuf. They are short-lived, bounded reducer outputs, and JSON keeps
atomic replacement and corruption handling simple. Query output remains raw
newline-delimited stdout because byte-offset pagination and zero-copy newline
counting are more valuable than a second normalized format.

## Concurrency, durability, and collection

The cache root has one exclusive process `LOCK`. Within a process,
`RwLock<Index>` protects only compact in-memory state and per-invocation mutexes
serialize mutations to the same invocation. No index lock spans an awaited
read, write, rename, permission update, directory creation, or recursive
deletion.

Lifecycle state is durable. Inspect/model-visible/progress counters are updated
in memory immediately and coalesced after 250 ms or into the next durable
mutation. A crash may lose the most recent telemetry interval, but cannot
regress committed lifecycle state. Atomic writes use a private temporary file
and rename. The contract covers process interruption; it does not claim
power-loss durability and therefore does not add an `fsync` to every short-lived
metadata update. Corrupt committed JSON fails closed.

GC accounts terminal bytes in the manifest. It protects live invocations and
unexpired deferred results, preserves the 80% low-water mark, and uses rename to
`trash/` as its deletion commit. The index entry is removed immediately after a
successful rename. Recursive unlink runs outside the index lock; a failed
unlink leaves trash absent from the live index and startup retries cleanup.

## Controlled filesystem comparison

Both release-mode runs used macOS arm64, 1,000,000 query rows (40 MB raw
stdout), 2,000 retained terminal invocations, 1,000 point lookups, and 2,000 GC
candidates. The baseline is clean commit
`0b1eb8d8087b665f232f9dfd0121af2c2c960685`; raw results and workload metadata
are checked in with the benchmark fixtures.

| Workload | Filesystem before | Optimized | Result |
| --- | ---: | ---: | ---: |
| Query count + 3-row sample | 118.540 ms | 2.732 ms | 43.4x faster |
| First 100-row page | 0.040 ms | 0.041 ms | unchanged |
| Continued 100-row page | not recorded | 0.037 ms | bounded seek |
| Rare filtered million-row scan | 126.810 ms | 114.390 ms | 9.8% faster |
| Point lookup p95 | 0.167 us | 0.167 us | unchanged |
| 2,000-record startup | 51.234 ms | 69.127 ms | 17.9 ms slower |
| 2,000-record store bytes | 3.24 MB | 2.21 MB | 31.9% less |
| Quota GC | 432.168 ms | 366.204 ms | 15.3% faster |
| GC shared-index write time | not recorded | 1.798 ms | 0.49% of GC |

One representative invocation uses four manifest commits—acceptance, starting,
running, and one coalesced terminal commit—writing 3,145 metadata bytes and
recounting evidence twice. The terminal storage finalization measured 0.375 ms
for build, 0.725 ms for 500 test results, and 3.143 ms for a million-row query
count plus commit.

| Writers | Throughput | p50 | p95 | p99 | Lookup p95 during writes | Inspect p95 during writes |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 991/s | 0.976 ms | 1.151 ms | 1.216 ms | 0.375 us | 0.459 us |
| 8 | 1,524/s | 5.208 ms | 6.462 ms | 7.088 ms | 0.417 us | 0.459 us |
| 32 | 959/s | 32.389 ms | 43.391 ms | 50.467 ms | 0.791 us | 0.917 us |

At 20,000 retained records, startup took 648.282 ms: 227.689 ms traversing
directories, 382.222 ms reading manifests, 21.050 ms decoding JSON, and
16.838 ms building indexes. The expanded benchmark took 34.187 seconds and its
storage workloads reached 80.4 MB peak RSS. The older schema did not record an
equivalent expanded-harness wall time or RSS, so those two values are reported
but are not used as comparative gates.

The optional 100,000-record run took 10.638 seconds to reopen 110.7 MB of
manifests: 1.463 seconds traversal, 8.836 seconds reads, 178.493 ms JSON decode,
and 158.850 ms index construction. That deliberately extreme cardinality is a
useful threshold for reconsidering a rebuildable startup snapshot, but not a
reason to add snapshot invalidation and checkpoint writes to the expected
short-lived workload. Even there, changing each manifest to protobuf would
address only 1.7% of startup time.

## Tailing and protocol-buffer decisions

Concurrent BEP tailing is not shipped. The largest checked Bazel fixture is
65,322 bytes / 158 events and reduces after exit in 0.111 ms. The concurrent
partial-frame experiment consumed all 158 events and finalized in 0.038 ms, a
0.073 ms saving. Even a synthetic 67.1 MB / 162,266-event stream reduces in
50.096 ms within the existing bounds. The representative saving is smaller than
the coordination, partial-frame, cancellation, and fallback complexity. The
same decision applies to live query counting: the optimized million-row
post-exit count is 2.732 ms, while the concurrent completed-line experiment
finalized in 0.050 ms. A roughly 2.7 ms saving does not justify live counter
recovery and cancellation state; direct-to-disk stdout preserves the simpler
design.

No new protobuf format is justified:

- BEP is already the correct varint-delimited protobuf stream.
- Raw logs and query stdout need cheap seeks and byte-offset cursors.
- At 2,000 records JSON decode is 2.479 ms of 69.127 ms startup; at 20,000 it
  is 21.050 ms of 648.282 ms. File reads and traversal, not JSON, dominate.
- Fixed binary cursors already remove JSON/base64 token overhead where the
  representation is model-visible.
- Detailed JSON sidecars are bounded and not read during startup. Framed
  protobuf should be reconsidered only if real retained detail cardinality
  makes sidecar decode or memory a measured bottleneck.

Log memory is adaptive: a complete BEP uses at most 2 MiB from each relevant
log; missing or partial BEP uses at most 8 MiB. Existing reducer fixtures and
goldens guard diagnostic quality.

## Reproduction

Build, run, and enforce the checked-in filesystem comparison gates with:

```sh
make bench-storage-compare
```

Override the workload through `STORAGE_BENCHMARK_ARGS`, for example:

```sh
make bench-storage STORAGE_BENCHMARK_ARGS='--label local --revision working-tree --query-rows 1000000 --invocations 2000 --lookup-samples 1000'
```

Use `--extended-startup` to add the optional 100,000-record startup workload.
Measurements are single controlled runs and should be treated as engineering
comparisons rather than statistical confidence intervals.
