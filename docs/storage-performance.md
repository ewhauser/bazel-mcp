# Storage design and performance

## Decision

Bazel MCP does not need a database for its short-lived invocation evidence.
The production store is a database-free filesystem design:

- Bazel writes stdout, stderr, and binary BEP directly to their final private
  files. These writes use the OS page cache; the MCP process does not pipe or
  retain a second raw-output buffer.
- UUIDv7 maps each invocation to
  `invocations/<day>/<16-way-shard>/<uuid>/`, providing deterministic lookup and
  bounded directory fan-out.
- `record.json` is a versioned atomic commit point. A cache-root `LOCK` permits
  one writer process, and startup rebuilds compact in-memory indexes from the
  retained time buckets.
- Query rows are not normalized or duplicated. Post-processing and inspection
  scan `stdout.log` with bounded lines and opaque byte-offset cursors. The
  runner redacts each row before filtering or returning it.
- BEP is decoded frame by frame into a bounded reducer accumulator. Complete
  protobuf frames are not retained in a stream-sized vector.
- GC uses per-record accounted bytes and an 80% low watermark. It never walks
  the entire cache during a normal pass. Terminal deletion atomically renames a
  directory into `trash/` before unlinking it; startup completes abandoned
  trash deletion. Live invocations and compact deferred results are protected;
  raw deferred evidence can be pruned independently.

There is no database migration or legacy-layout reader because no released
installation depends on the previous layout.

## Controlled comparison

Both release-mode runs used macOS arm64, 1,000,000 query rows (40 MB raw
stdout), 2,000 retained terminal invocations, 1,000 point lookups, and 2,000 GC
candidates. The baseline was captured from clean commit
`2cdf8e9d95bf6f1123d5ec3336f8bdc2da2f28aa` before replacement. Raw normalized
results are checked in beside the benchmark fixtures.

| Workload | Embedded database | Filesystem | Change |
| --- | ---: | ---: | ---: |
| Query store bytes | 186.0 MB | 40.0 MB | 78.5% less |
| Query post-process | 9,302.5 ms | 117.6 ms | 79.1x faster |
| First 100-row page | 142.3 ms | 0.046 ms | 3,100x faster |
| Rare filtered page | 1,341.2 ms | 110.3 ms | 12.2x faster |
| Point lookup p50 | 23.292 us | 0.125 us | 186x faster |
| Point lookup p95 | 25.417 us | 0.167 us | 152x faster |
| 2,000-record startup | 27.4 ms | 48.7 ms | 21.3 ms slower |
| 2,000-record store bytes | 9.34 MB | 3.24 MB | 65.3% less |
| Quota GC | 864.9 ms | 434.4 ms | 1.99x faster |
| Whole benchmark wall time | 18.79 s | 7.64 s | 2.46x faster |
| Peak memory footprint | 18.40 MB | 8.95 MB | 51.4% less |

Startup is the only measured regression. It is bounded by the number of
retained records and costs 48.7 ms for 2,000 records on this machine. That
21.3 ms absolute cost is acceptable for a server startup path, especially given
the removal of database allocation, migration, and failure modes. Terminal
records trust atomically committed byte accounting, so startup performs one
record read rather than restatting every evidence file.

The old quota pass ended at 9.01 MB against an 8.77 MB target. The filesystem
pass crossed its high watermark, deleted to the configured 80% low watermark,
and ended at 4.57 MB against a 5.72 MB high watermark. Directory renames make
the deletion state recoverable without a database transaction.

## Reproduction

Build, run, and enforce the checked-in comparison gates with:

```sh
make bench-storage-compare
```

Override the workload through `STORAGE_BENCHMARK_ARGS`, for example:

```sh
make bench-storage STORAGE_BENCHMARK_ARGS='--label local --query-rows 1000000 --invocations 2000 --lookup-samples 1000'
```

For process-level memory and wall-time accounting on macOS, prefix the release
binary with `/usr/bin/time -l`. Measurements are single controlled runs and
should be treated as engineering comparisons rather than statistical
confidence intervals.
