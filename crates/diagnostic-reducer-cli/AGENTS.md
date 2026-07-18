# Diagnostic reducer CLI instructions

This crate is a thin I/O and presentation adapter over the reusable reducer
crates. Do not put parser, ranking, redaction, or budget logic here. Keep stdin
and file acquisition incremental, never launch commands through a shell, keep
stdout machine-readable for JSON, JSONL, SARIF, and GitHub formats, and send
operational errors to stderr.
