# diagnostic-reducer-core

`diagnostic-reducer-core` is a synchronous, provider-neutral state machine for
turning bounded log streams and trusted structured findings into deterministic
diagnostics. It performs no I/O and has no async runtime, command runner,
storage, network, environment, or provider-protocol dependency.

The core owns:

- caller-defined scopes and stdout/stderr/combined/annotation streams;
- chunk-invariant CR, LF, and CRLF framing with explicit line and scope bounds;
- a fixed, ordered `ParserPlan` of trusted native parsers;
- parser lifecycle events for complete, truncated, cancelled, and interrupted
  scopes;
- provenance, exact deduplication, typed ranking, and serialized-output budgets;
- caller-supplied path mapping and redaction, in that order.

It deliberately contains no compiler or test-framework grammar. Use the
`diagnostic-reducer` crate for the built-in parser pack, or implement `Parser`
for a domain-specific pack.

```rust
use diagnostic_reducer_core::{
    Budget, Diagnostic, DiagnosticClass, Emitter, EndReason, GenericRanker,
    LogLine, NoPathMapping, NoRedaction, OutputPolicy, Parser, ParserPlan,
    ReductionSession, Scope, SessionOptions, Severity, Stream,
};

struct PanicParser;

impl Parser for PanicParser {
    fn id(&self) -> &'static str {
        "example.panic.v1"
    }

    fn observe(&mut self, line: &LogLine<'_>, output: &mut Emitter<'_>) {
        if line.text.contains("panicked at") {
            output.diagnostic(Diagnostic {
                severity: Severity::Error,
                class: DiagnosticClass::Test,
                code: Some("panic".into()),
                message: line.text.into(),
                location: None,
                provenance: None,
                quality: Default::default(),
                repetition_count: 1,
            });
        }
    }
}

let mut plan = ParserPlan::new();
plan.push(PanicParser)?;

let mut session = ReductionSession::new(
    plan,
    SessionOptions {
        budget: Budget { max_bytes: 4096, max_items: 20 },
        ..SessionOptions::default()
    },
    OutputPolicy::new(&NoRedaction, &NoPathMapping, &GenericRanker),
);
session.begin_scope(Scope::step("unit-tests"));
session.push_chunk("unit-tests", Stream::Stderr, b"thread 'x' panicked at src/lib.rs\n");
session.end_scope("unit-tests", EndReason::Complete);
let result = session.finish();
# Ok::<(), diagnostic_reducer_core::ParserPlanError>(())
```

Parser plans are constructed before input is accepted; empty and duplicate IDs
are rejected. Parsers are synchronous and deterministic. The engine bounds
retained bytes, line length, open scopes, and emitted candidates before the
final item and serialized-byte budgets are applied. `ReductionStats` contains
only counters and never raw log text.

Raw log retention is a caller responsibility. The returned `Reduction` has
already passed through path mapping, redaction, control-character sanitization,
deduplication, ranking, and budgeting.
