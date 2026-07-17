# Recorded reducer corpus

Each nested `case.toml` points at a real target under `examples/` and declares a
semantic reducer contract. Its sibling files contain sanitized BEP, stdout,
stderr, optional test-log evidence, the exact canonical result, and recording
provenance.

Use the `bazel-mcp-reducer-cases` harness to discover and replay cases. Never
overwrite a golden with a live run: record to `actual.*`, review the full diff,
then use the explicitly gated accept command documented in
[`docs/reducer-integration-testing.md`](../../docs/reducer-integration-testing.md).
