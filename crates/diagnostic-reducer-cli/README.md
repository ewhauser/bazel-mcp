# diagnostic-reduce

`diagnostic-reduce` incrementally parses generic compiler, test, lint, and tool
logs from stdin or files. It is a presentation adapter over
`diagnostic-reducer-core` and `diagnostic-reducer`; it does not launch commands
or retain raw evidence.

```console
some-command 2>&1 | diagnostic-reduce --format human
diagnostic-reduce build.log --format sarif > diagnostics.sarif
diagnostic-reduce test.log --format github --fail-on error
```

Formats are `human`, `json`, `jsonl`, `sarif`, and `github`. Repeated
`--redact-literal` arguments replace matching output text before findings are
deduplicated or serialized. Repeated `--strip-prefix` arguments provide a pure
CI workspace-path projection without giving the reducer filesystem access.
