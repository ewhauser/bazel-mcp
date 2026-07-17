//! Asynchronous Bazel invocation lifecycle and application service.

mod artifacts;
mod cancel;
mod capture;
mod evidence;
mod execution;
mod inspection;
mod output_base_lock;
mod scheduler;
mod service;
mod test_evidence;
mod version;

pub use bazel_mcp_reducer::{StarlarkLimits, StarlarkReducerConfig};
pub use inspection::{InspectRequest, InspectResult, InspectView};
#[doc(hidden)]
pub use service::RunnerTestSupport;
pub use service::{
    BepTransport, CancelResult, InvocationProgress, InvocationService, RunnerConfig, RunnerError,
};
