# Source-agnostic diagnostic reduction

The workspace contains two reducer layers:

- `diagnostic-reducer` owns terminal normalization, built-in text parsers,
  generic diagnostics, redaction, exact deduplication, ranking, and bounded
  output.
- `bazel-mcp-reducer` adapts those diagnostics to Bazel categories and combines
  them with BEP events, targets, actions, artifacts, test metadata, custom
  reducers, and Bazel-specific headlines.

The core accepts ordinary byte slices, so CI steps, local task runners, and
non-Bazel build systems can use it without constructing BEP events or an
invocation summary. See `crates/diagnostic-reducer/README.md` for the public API
and a minimal pinned-Git example.

The generic core has no persistence API. Callers preserve raw evidence under
their own authorization policy and persist only the redacted `Reduction` when
appropriate. The Bazel runner continues to retain raw invocation evidence
locally and applies its summary sanitization as a defense-in-depth boundary.

Large-log segmentation, provider-specific acquisition, structured workflow
annotations, and streaming segment APIs are separate concerns. Callers should
select and bound relevant text before calling the synchronous reduction API.
