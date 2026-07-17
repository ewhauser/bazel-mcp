# Bazel core reducer examples

This workspace exercises Bazel-owned loading, visibility, analysis, action, and
test outcome evidence without depending on a language toolchain.

`//:success` is a normal successful target. Targets under `//cases` intentionally
fail and are tagged for explicit reducer-harness execution.
