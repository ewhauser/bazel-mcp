# Diagnostic reducer core instructions

This crate is the extraction boundary for the reusable log-reduction engine.
It must remain synchronous, deterministic, bounded, and free of provider,
build-system, CI, process, filesystem, network, async-runtime, clock, and
randomness dependencies. Keep parser plans immutable, framing chunk-invariant,
raw parser state bounded, redaction before deduplication and serialization, and
all output budgets explicit.
