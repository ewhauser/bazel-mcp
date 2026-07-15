use std::time::Duration;

use tokio::process::Child;

use crate::RunnerError;

/// Last-resort cleanup for a Bazel process group when an invocation future is
/// dropped (for example, because the MCP transport or server is shutting down).
pub(crate) struct ProcessGroupGuard {
    #[cfg(unix)]
    process_group: Option<i32>,
}

impl ProcessGroupGuard {
    pub(crate) fn for_child(child: &Child) -> Self {
        Self {
            #[cfg(unix)]
            process_group: child.id().and_then(|id| i32::try_from(id).ok()),
        }
    }

    pub(crate) fn disarm(&mut self) {
        #[cfg(unix)]
        {
            self.process_group = None;
        }
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(process_group) = self.process_group {
            use nix::{
                sys::signal::{Signal, kill},
                unistd::Pid,
            };
            let _ = kill(Pid::from_raw(-process_group), Signal::SIGKILL);
        }
    }
}

pub(crate) async fn terminate_child(
    child: &mut Child,
    interrupt_grace: Duration,
    terminate_grace: Duration,
) -> Result<std::process::ExitStatus, RunnerError> {
    #[cfg(unix)]
    let process_group = child.id().and_then(|id| i32::try_from(id).ok());
    signal_interrupt(child);
    let status = match tokio::time::timeout(interrupt_grace, child.wait()).await {
        Ok(status) => status?,
        Err(_) => {
            signal_terminate(child);
            match tokio::time::timeout(terminate_grace, child.wait()).await {
                Ok(status) => status?,
                Err(_) => {
                    signal_kill(child);
                    child.wait().await?
                }
            }
        }
    };
    // A shell/wrapper may exit on SIGINT while a background descendant ignores
    // it. The direct child is no longer available to identify its group after
    // wait(), so retain the pgid and make cancellation terminal for the group.
    #[cfg(unix)]
    if let Some(process_group) = process_group {
        signal_process_group(process_group, nix::sys::signal::Signal::SIGKILL);
    }
    Ok(status)
}

#[cfg(unix)]
fn signal_process_group(process_group: i32, signal: nix::sys::signal::Signal) {
    use nix::{sys::signal::kill, unistd::Pid};
    let _ = kill(Pid::from_raw(-process_group), signal);
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
