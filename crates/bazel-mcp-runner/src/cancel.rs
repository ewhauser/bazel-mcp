use std::time::Duration;

use tokio::process::Child;

use crate::RunnerError;

pub(crate) async fn terminate_child(
    child: &mut Child,
    interrupt_grace: Duration,
    terminate_grace: Duration,
) -> Result<std::process::ExitStatus, RunnerError> {
    signal_interrupt(child);
    match tokio::time::timeout(interrupt_grace, child.wait()).await {
        Ok(status) => Ok(status?),
        Err(_) => {
            signal_terminate(child);
            match tokio::time::timeout(terminate_grace, child.wait()).await {
                Ok(status) => Ok(status?),
                Err(_) => {
                    signal_kill(child);
                    Ok(child.wait().await?)
                }
            }
        }
    }
}

#[cfg(unix)]
fn signal_interrupt(child: &mut Child) {
    use nix::{
        sys::signal::{Signal, kill},
        unistd::Pid,
    };
    if let Some(id) = child.id().and_then(|id| i32::try_from(id).ok()) {
        let _ = kill(Pid::from_raw(-id), Signal::SIGINT);
    }
}

#[cfg(not(unix))]
fn signal_interrupt(child: &mut Child) {
    let _ = child.start_kill();
}

#[cfg(unix)]
fn signal_terminate(child: &mut Child) {
    use nix::{
        sys::signal::{Signal, kill},
        unistd::Pid,
    };
    if let Some(id) = child.id().and_then(|id| i32::try_from(id).ok()) {
        let _ = kill(Pid::from_raw(-id), Signal::SIGTERM);
    }
}

#[cfg(not(unix))]
fn signal_terminate(child: &mut Child) {
    let _ = child.start_kill();
}

#[cfg(unix)]
fn signal_kill(child: &mut Child) {
    use nix::{
        sys::signal::{Signal, kill},
        unistd::Pid,
    };
    if let Some(id) = child.id().and_then(|id| i32::try_from(id).ok()) {
        let _ = kill(Pid::from_raw(-id), Signal::SIGKILL);
    }
}

#[cfg(not(unix))]
fn signal_kill(child: &mut Child) {
    let _ = child.start_kill();
}
