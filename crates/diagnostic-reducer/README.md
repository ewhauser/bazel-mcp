# diagnostic-reducer

`diagnostic-reducer` synchronously turns one or more command-output byte streams
into deterministic, redacted, and bounded compiler, test, and tool diagnostics.
It performs no I/O and does not depend on Bazel protocol types, MCP, storage, a
command runner, or an async runtime.

The crate is not published independently. An external consumer can pin a
repository revision:

```toml
[dependencies]
diagnostic-reducer = { git = "https://github.com/ewhauser/bazel-mcp.git", rev = "<commit>" }
```

```rust
use diagnostic_reducer::{
    Budget, NoRedaction, ReductionOptions, TextInput, reduce,
};

let stderr = b"src/main.go:12:4: undefined: total";
let result = reduce(
    &[TextInput::new(stderr)],
    &ReductionOptions {
        budget: Budget {
            max_bytes: 4096,
            max_items: 20,
        },
        ..ReductionOptions::default()
    },
    &NoRedaction,
);

assert_eq!(result.diagnostics[0].message, "undefined: total");
assert_eq!(
    result.diagnostics[0].location.as_ref().unwrap().path,
    "src/main.go"
);
```

Production consumers should normally supply a `Redactor` instead of
`NoRedaction`. Redaction is applied to messages, paths, and provenance before
exact deduplication, ranking, serialized diagnostic byte accounting, or return.

Input slice order is stable and authoritative. Exact duplicates with the same
provenance aggregate their `repetition_count`; diagnostics with different
provenance remain distinct. The built-in parser registry is fixed so parser
precedence cannot depend on runtime registration order.
