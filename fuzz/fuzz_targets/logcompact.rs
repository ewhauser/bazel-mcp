#![no_main]
use libfuzzer_sys::fuzz_target;
use logcompact_builtins as logcompact;

fuzz_target!(|data: &[u8]| {
    let normalized = logcompact::normalize_terminal_text(data);
    let _ = logcompact::deduplicate_lines(&normalized);
    let _ = logcompact::reduce(
        &[logcompact::TextInput::new(data)],
        &logcompact::ReductionOptions {
            budget: logcompact::Budget {
                max_bytes: 4 * 1024,
                max_items: 20,
            },
            ..logcompact::ReductionOptions::default()
        },
        &logcompact::NoRedaction,
    );

    let mut session = logcompact::ReductionSession::new(
        logcompact::builtin_parser_plan(logcompact::BuiltinParserOptions::default()),
        logcompact::SessionOptions {
            budget: logcompact::Budget {
                max_bytes: 4 * 1024,
                max_items: 20,
            },
            limits: logcompact::Limits {
                max_scope_bytes: 64 * 1024,
                max_line_bytes: 4 * 1024,
                max_candidates: 100,
                ..logcompact::Limits::default()
            },
        },
        logcompact::OutputPolicy::new(
            &logcompact::NoRedaction,
            &logcompact::NoPathMapping,
            &logcompact::GenericRanker,
        ),
    );
    session.begin_scope(logcompact::Scope::step("fuzz"));
    let chunk_size = data
        .first()
        .map_or(1, |byte| usize::from(*byte).saturating_add(1));
    for chunk in data.chunks(chunk_size) {
        session.push_chunk("fuzz", logcompact::Stream::Combined, chunk);
    }
    session.end_scope("fuzz", logcompact::EndReason::Complete);
    let _ = session.finish();
});
