//! Loopback Build Event Service capture backed by Buffa protocol views.

#[doc(hidden)]
pub mod codec;
mod server;

pub mod proto {
    #![allow(clippy::match_single_binding)]

    buffa::include_proto!("google.devtools.build.v1");
}

pub use server::{BesCapture, BesError, BesServer, BesStreamEvent, CaptureStats};
