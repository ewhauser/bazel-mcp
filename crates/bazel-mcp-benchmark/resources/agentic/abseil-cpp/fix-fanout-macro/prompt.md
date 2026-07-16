The `fanout_suite` Bazel macro lost a common dependency, so a single build now
produces many downstream C++ compilation failures. Before editing any file,
reproduce the failure by building
`//bazel_mcp_agentic_fanout:fanout_app` with `--keep_going`.

Repair the shared macro in `defs.bzl` so every generated library declares the
dependency required by `unit.cc`. Fix only the macro; do not edit the BUILD or
C++ sources. After the fix, rerun the same `--keep_going` build.
