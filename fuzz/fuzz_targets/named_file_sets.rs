#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let framed = frame(data);
    let partial = bazel_mcp_bep::decode_stream_partial(framed.as_slice(), 1024 * 1024);
    let _ = bazel_mcp_reducer::reduce_artifacts(&partial.events);
});

fn frame(data: &[u8]) -> Vec<u8> {
    let mut framed = Vec::with_capacity(data.len().saturating_add(10));
    let mut remaining = data.len();
    while remaining >= 0x80 {
        let byte = u8::try_from(remaining & 0x7f).expect("length byte is masked to seven bits");
        framed.push(byte | 0x80);
        remaining >>= 7;
    }
    framed.push(u8::try_from(remaining).expect("final length byte fits in seven bits"));
    framed.extend_from_slice(data);
    framed
}
