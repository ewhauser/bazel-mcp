//! Bazel Build Event Protocol framing and the generated wire-compatible model.

mod framing;

pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/bazel_mcp.bep.rs"));
}

pub use framing::{
    DEFAULT_MAX_FRAME_BYTES, DEFAULT_MAX_STREAM_BYTES, DEFAULT_MAX_STREAM_EVENTS, FrameError,
    PartialStream, decode_stream, decode_stream_partial, decode_stream_partial_bounded,
    encode_frame, read_frame,
};
