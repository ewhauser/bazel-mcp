#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(event) = bazel_mcp_bep::decode_event(data) {
        let _ = bazel_mcp_reducer::reduce_artifacts(&[event]);
    }
});
