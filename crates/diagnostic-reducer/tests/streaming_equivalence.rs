use diagnostic_reducer::{
    Budget, BuiltinParserOptions, EndReason, GenericRanker, NoPathMapping, NoRedaction,
    OutputPolicy, Reduction, ReductionOptions, ReductionSession, Scope, SessionOptions, Stream,
    TextInput, builtin_parser_plan, reduce,
};

const COMPILER_CASES: &[&[u8]] = &[
    b"error[E0308]: mismatched types\n --> src/lib.rs:7:5\n  |\n7 | value\n  | ^ expected u32, found &str\n",
    b"Traceback (most recent call last):\n  File \"src/pricing.py\", line 19, in total\nValueError: unsupported currency\n",
    b"src/main.go:12:4: undefined: total\n",
    b"src/schema.proto:4:2: warning: Import common.proto is unused.\n",
    b"\x1b[31msrc/app.ts(8,11): error TS2322: Type 'number' is not assignable to type 'string'.\x1b[0m\r\n",
];

fn streaming(input: &[u8], chunk_size: usize) -> Reduction {
    let mut session = ReductionSession::new(
        builtin_parser_plan(BuiltinParserOptions::default()),
        SessionOptions {
            budget: Budget::unbounded(),
            ..SessionOptions::default()
        },
        OutputPolicy::new(&NoRedaction, &NoPathMapping, &GenericRanker),
    );
    session.begin_scope(Scope::step("fixture"));
    for chunk in input.chunks(chunk_size) {
        session.push_chunk("fixture", Stream::Stderr, chunk);
    }
    session.end_scope("fixture", EndReason::Complete);
    session.finish()
}

fn without_provenance(mut reduction: Reduction) -> Reduction {
    for diagnostic in &mut reduction.diagnostics {
        diagnostic.provenance = None;
    }
    reduction.stats = Default::default();
    reduction
}

#[test]
fn batch_and_streaming_diagnostics_match_for_every_chunk_width() {
    for input in COMPILER_CASES {
        let batch = reduce(
            &[TextInput::new(input)],
            &ReductionOptions {
                budget: Budget::unbounded(),
                ..ReductionOptions::default()
            },
            &NoRedaction,
        );
        for chunk_size in 1..=input.len().min(64) {
            let stream = without_provenance(streaming(input, chunk_size));
            assert_eq!(
                batch.diagnostics,
                stream.diagnostics,
                "chunk size {chunk_size} changed the reduction for {:?}",
                String::from_utf8_lossy(input)
            );
        }
    }
}

#[test]
fn test_state_is_chunk_invariant_and_scope_isolated() {
    let input = b"test invoice::fails ... FAILED\n---- invoice::fails stdout ----\nthread 'invoice::fails' panicked at src/lib.rs:7:3:\nassertion `left == right` failed\n";
    let expected = streaming(input, input.len());
    for chunk_size in 1..=32 {
        let actual = streaming(input, chunk_size);
        assert_eq!(expected.test_failures, actual.test_failures);
    }
}
