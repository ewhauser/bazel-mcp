# Diagnostic reducer crate instructions

This crate is source-agnostic, synchronous, deterministic, and bounded. It must
not depend on Bazel protocol/domain crates, MCP, storage, the command runner, an
async runtime, the filesystem, environment variables, the network, clocks, or
randomness. Preserve parser order, exact deduplication, stable ordering, control
sanitization, redaction-before-return, and serialized diagnostic byte budgets.
