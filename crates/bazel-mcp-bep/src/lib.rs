//! Bazel Build Event Protocol framing and the generated wire-compatible model.

mod framing;

pub mod proto {
    #![allow(clippy::match_single_binding)]

    buffa::include_proto!("bazel_mcp.bep");
}

/// Stable re-exports of the generated borrowed BEP model used by reducers.
pub mod view {
    pub use crate::proto::{BuildEventIdView, BuildEventView, FileView, NamedSetOfFilesView};

    pub mod build_event {
        pub use crate::proto::build_event::PayloadView as Payload;
    }

    pub mod build_event_id {
        pub use crate::proto::build_event_id::{IdView as Id, NamedSetOfFilesIdView};
    }

    pub mod file {
        pub use crate::proto::file::FileView as File;
    }
}

pub use framing::{
    BepEvent, BorrowedBepEvent, DEFAULT_MAX_FRAME_BYTES, DEFAULT_MAX_STREAM_BYTES,
    DEFAULT_MAX_STREAM_EVENTS, FrameError, IncrementalStreamControl, IncrementalStreamDecoder,
    PartialStream, StreamOutcome, decode_event_id, decode_stream, decode_stream_partial,
    encode_event_id, encode_frame, read_frame, visit_stream_partial_borrowed_bounded,
};
