#![no_main]
use bazel_mcp_bep::proto::BuildEvent;
use libfuzzer_sys::fuzz_target;
use prost::Message;

fuzz_target!(|data: &[u8]| {
    if let Ok(event) = BuildEvent::decode(data) {
        let _ = bazel_mcp_reducer::reduce_artifacts(&[event]);
    }
});
