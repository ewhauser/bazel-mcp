#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|value: &str| {
    let _ = bazel_mcp_store::cursor_is_well_formed(value);
});
