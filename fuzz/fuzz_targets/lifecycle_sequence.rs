#![no_main]
use bazel_mcp_types::InvocationState;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|sequence: &[u8]| {
    let mut state = InvocationState::Queued;
    for byte in sequence {
        let next = match byte % 7 {
            0 => InvocationState::Queued,
            1 => InvocationState::Running,
            2 => InvocationState::Succeeded,
            3 => InvocationState::Failed,
            4 => InvocationState::Cancelled,
            5 => InvocationState::TimedOut,
            _ => InvocationState::Interrupted,
        };
        if state.validate_transition(next).is_ok() {
            state = next;
        }
    }
});
