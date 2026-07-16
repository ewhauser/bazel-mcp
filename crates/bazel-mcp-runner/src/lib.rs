//! Asynchronous Bazel invocation lifecycle and application service.

mod cancel;
mod capture;
mod service;
mod version;

pub use bazel_mcp_reducer::{StarlarkLimits, StarlarkReducerConfig};
pub use service::{
    BepTransport, CancelResult, InspectRequest, InspectResult, InspectView, InvocationService,
    RunnerConfig, RunnerError,
};
