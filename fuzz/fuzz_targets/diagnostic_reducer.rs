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
});
