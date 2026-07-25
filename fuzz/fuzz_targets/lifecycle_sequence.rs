#![no_main]
use std::path::PathBuf;

use bazel_mcp_types::{BazelCommand, InvocationRecord, InvocationRequest, InvocationState};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|sequence: &[u8]| {
    let request = InvocationRequest::new(PathBuf::new(), BazelCommand::Build, Vec::new());
    let mut record = InvocationRecord::queued(request);
    for byte in sequence {
        let next = match byte % 8 {
            0 => InvocationState::Queued,
            1 => InvocationState::Starting,
            2 => InvocationState::Running,
            3 => InvocationState::Succeeded,
            4 => InvocationState::Failed,
            5 => InvocationState::Cancelled,
            6 => InvocationState::TimedOut,
            _ => InvocationState::Interrupted,
        };
        let _ = record.transition(next);
    }
});
