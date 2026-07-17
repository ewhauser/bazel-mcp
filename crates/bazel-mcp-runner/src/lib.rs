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

pub use bazel_mcp_reducer::{RawStarlarkConfig, StarlarkLimits, StarlarkReducerConfig};
pub use bazel_mcp_types::{InspectResult, InspectView};
pub use inspection::InspectRequest;
#[doc(hidden)]
pub use service::RunnerTestSupport;
pub use service::{
    BepTransport, CancelResult, InvocationProgress, InvocationService, RawRunnerConfig,
    RunnerConfig, RunnerError,
};
