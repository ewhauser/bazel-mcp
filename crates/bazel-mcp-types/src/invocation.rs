use std::{collections::BTreeMap, fmt, path::PathBuf, time::SystemTime};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::{BazelCommand, InvocationSummary, RunSummary};

/// Stable, opaque identity for one Bazel invocation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct InvocationId(Uuid);

impl InvocationId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    #[must_use]
    pub const fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl Default for InvocationId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for InvocationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Milliseconds since the Unix epoch.
pub fn unix_timestamp_ms() -> i64 {
    let millis = SystemTime::UNIX_EPOCH
        .elapsed()
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    i64::try_from(millis).unwrap_or(i64::MAX)
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InvocationRequest {
    pub id: InvocationId,
    pub workspace: PathBuf,
    #[serde(default)]
    pub startup_arguments: Vec<String>,
    pub command: BazelCommand,
    #[serde(default)]
    pub arguments: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Arguments passed to the executed program after Bazel's `--` delimiter.
    ///
    /// These values are always treated as sensitive and must be replaced before
    /// an invocation record is persisted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub program_arguments: Vec<String>,
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    pub requested_at_ms: i64,
}

impl InvocationRequest {
    #[must_use]
    pub fn new(workspace: PathBuf, command: BazelCommand, arguments: Vec<String>) -> Self {
        Self {
            id: InvocationId::new(),
            workspace,
            startup_arguments: Vec::new(),
            command,
            arguments,
            target: None,
            program_arguments: Vec::new(),
            timeout_ms: None,
            environment: BTreeMap::new(),
            requested_at_ms: unix_timestamp_ms(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvocationState {
    Queued,
    Starting,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    TimedOut,
    Interrupted,
}

impl InvocationState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed_out",
            Self::Interrupted => "interrupted",
        }
    }

    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::TimedOut | Self::Interrupted
        )
    }

    fn validate_transition(self, next: Self) -> Result<(), StateTransitionError> {
        let valid = matches!(
            (self, next),
            (
                Self::Queued,
                Self::Starting | Self::Cancelled | Self::Interrupted
            ) | (
                Self::Starting,
                Self::Running | Self::Failed | Self::Cancelled | Self::TimedOut | Self::Interrupted
            ) | (
                Self::Running,
                Self::Succeeded
                    | Self::Failed
                    | Self::Cancelled
                    | Self::TimedOut
                    | Self::Interrupted
            )
        );
        if valid {
            Ok(())
        } else {
            Err(StateTransitionError {
                current: self,
                next,
            })
        }
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("invalid invocation state transition from {current:?} to {next:?}")]
pub struct StateTransitionError {
    pub current: InvocationState,
    pub next: InvocationState,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Termination {
    Exit { code: i32 },
    Signal { signal: i32 },
    Timeout,
    OutputLimit { maximum_bytes: u64 },
    Cancelled,
    SpawnFailure { message: String },
    Interrupted,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct InvocationMetrics {
    #[serde(default)]
    pub raw_output_bytes: u64,
    pub bep_bytes: u64,
    pub bep_events: u64,
    pub model_visible_bytes: u64,
    pub progress_notifications: u64,
    pub inspect_calls: u64,
    pub queue_ms: u64,
    #[serde(default)]
    pub output_base_lock_wait_ms: u64,
    pub bazel_wall_ms: u64,
    pub reduction_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InvocationRecord {
    pub request: InvocationRequest,
    pub state: InvocationState,
    pub started_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
    pub termination: Option<Termination>,
    pub summary: Option<InvocationSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunSummary>,
    pub metrics: InvocationMetrics,
    #[serde(default)]
    pub canonical_arguments: Option<Vec<String>>,
    #[serde(default)]
    pub cancellation_reason: Option<String>,
}

impl InvocationRecord {
    #[must_use]
    pub fn queued(request: InvocationRequest) -> Self {
        Self {
            request,
            state: InvocationState::Queued,
            started_at_ms: None,
            finished_at_ms: None,
            termination: None,
            summary: None,
            run: None,
            metrics: InvocationMetrics::default(),
            canonical_arguments: None,
            cancellation_reason: None,
        }
    }

    pub fn transition(&mut self, next: InvocationState) -> Result<(), StateTransitionError> {
        self.state.validate_transition(next)?;
        let now = unix_timestamp_ms();
        if next == InvocationState::Running {
            self.started_at_ms = Some(now);
        }
        if next.is_terminal() {
            self.finished_at_ms = Some(now);
        }
        self.state = next;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_allows_only_monotonic_transitions() {
        assert!(
            InvocationState::Queued
                .validate_transition(InvocationState::Starting)
                .is_ok()
        );
        assert!(
            InvocationState::Starting
                .validate_transition(InvocationState::Running)
                .is_ok()
        );
        assert!(
            InvocationState::Running
                .validate_transition(InvocationState::Failed)
                .is_ok()
        );
        assert!(
            InvocationState::Succeeded
                .validate_transition(InvocationState::Running)
                .is_err()
        );
        assert!(
            InvocationState::Queued
                .validate_transition(InvocationState::Succeeded)
                .is_err()
        );
    }

    #[test]
    fn version_seven_ids_sort_by_creation_time() {
        let first = InvocationId::new();
        let second = InvocationId::new();
        assert!(first <= second);
    }
}
