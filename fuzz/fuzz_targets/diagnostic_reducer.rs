#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let normalized = diagnostic_reducer::normalize_terminal_text(data);
    let _ = diagnostic_reducer::deduplicate_lines(&normalized);
    let _ = diagnostic_reducer::reduce(
        &[diagnostic_reducer::TextInput::new(data)],
        &diagnostic_reducer::ReductionOptions {
            budget: diagnostic_reducer::Budget {
                max_bytes: 4 * 1024,
                max_items: 20,
            },
            ..diagnostic_reducer::ReductionOptions::default()
        },
        &diagnostic_reducer::NoRedaction,
    );

    let mut session = diagnostic_reducer::ReductionSession::new(
        diagnostic_reducer::builtin_parser_plan(
            diagnostic_reducer::BuiltinParserOptions::default(),
        ),
        diagnostic_reducer::SessionOptions {
            budget: diagnostic_reducer::Budget {
                max_bytes: 4 * 1024,
                max_items: 20,
            },
            limits: diagnostic_reducer::Limits {
                max_scope_bytes: 64 * 1024,
                max_line_bytes: 4 * 1024,
                max_candidates: 100,
                ..diagnostic_reducer::Limits::default()
            },
        },
        diagnostic_reducer::OutputPolicy::new(
            &diagnostic_reducer::NoRedaction,
            &diagnostic_reducer::NoPathMapping,
            &diagnostic_reducer::GenericRanker,
        ),
    );
    session.begin_scope(diagnostic_reducer::Scope::step("fuzz"));
    let chunk_size = data
        .first()
        .map_or(1, |byte| usize::from(*byte).saturating_add(1));
    for chunk in data.chunks(chunk_size) {
        session.push_chunk("fuzz", diagnostic_reducer::Stream::Combined, chunk);
    }
    session.end_scope("fuzz", diagnostic_reducer::EndReason::Complete);
    let _ = session.finish();
});
