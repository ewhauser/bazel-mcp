//! Asynchronous Bazel invocation lifecycle and application service.

mod cancel;
mod capture;
mod output_base_lock;
mod service;
mod version;

pub use bazel_mcp_reducer::{StarlarkLimits, StarlarkReducerConfig};
pub use service::{
    BepTransport, CancelResult, InspectRequest, InspectResult, InspectView, InvocationProgress,
    InvocationService, RunnerConfig, RunnerError,
};
