#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = bazel_mcp_bep::decode_stream_partial(data, 1024 * 1024);
});
