#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let normalized = bazel_mcp_reducer::normalize_terminal_text(data);
    let _ = bazel_mcp_reducer::deduplicate_lines(&normalized);
});
