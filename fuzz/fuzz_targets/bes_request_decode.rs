#![no_main]

use bazel_mcp_bes::{
    codec::DecodeOwnedView, proto::PublishBuildToolEventStreamRequestOwnedView,
};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = <PublishBuildToolEventStreamRequestOwnedView as DecodeOwnedView>::decode(
        buffa::bytes::Bytes::copy_from_slice(data),
    );
});
