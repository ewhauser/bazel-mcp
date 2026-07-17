//! Protocol-neutral task application state derived from durable runner data.

use bazel_mcp_types::{
    DeferredFailureKind, DeferredResultView, DeferredRetrieval, DeferredTerminalState,
    InvocationId, InvocationState, unix_timestamp_ms,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TaskState {
    Working,
    Completed,
    Failed,
    Cancelled,
}

impl TaskState {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub(crate) const fn is_terminal(self) -> bool {
        !matches!(self, Self::Working)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TaskResultState {
    Pending,
    Invocation { tool_error: bool },
    InternalFailure { redacted_message: String },
    Cancelled,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TaskSnapshot {
    pub(crate) id: InvocationId,
    pub(crate) state: TaskState,
    pub(crate) result: TaskResultState,
    pub(crate) created_at_ms: i64,
    pub(crate) updated_at_ms: i64,
    pub(crate) ttl_ms: u64,
    pub(crate) poll_interval_ms: u64,
    pub(crate) status_message: String,
}

impl TaskSnapshot {
    #[must_use]
    pub(crate) fn from_view(view: &DeferredResultView, poll_interval_ms: u64) -> Self {
        let internal_failure = view
            .deferred
            .failure
            .as_ref()
            .filter(|failure| failure.kind == DeferredFailureKind::Internal);
        let legacy_cancelled = view.deferred.retrieval == DeferredRetrieval::SeparateResult
            && view.deferred.terminal_override == Some(DeferredTerminalState::Cancelled);
        let (state, result) = if legacy_cancelled {
            (
                TaskState::Cancelled,
                TaskResultState::Invocation {
                    tool_error: view.deferred.failure.is_some(),
                },
            )
        } else if let Some(failure) = internal_failure {
            (
                TaskState::Failed,
                TaskResultState::InternalFailure {
                    redacted_message: failure.redacted_message.clone(),
                },
            )
        } else {
            match view.deferred.retrieval {
                DeferredRetrieval::InlineResult
                    if view.invocation.state == InvocationState::Cancelled =>
                {
                    (TaskState::Cancelled, TaskResultState::Cancelled)
                }
                DeferredRetrieval::SeparateResult if view.deferred.failure.is_some() => (
                    TaskState::Failed,
                    TaskResultState::Invocation { tool_error: true },
                ),
                _ if view.invocation.state.is_terminal() => (
                    TaskState::Completed,
                    TaskResultState::Invocation {
                        tool_error: view.deferred.failure.is_some(),
                    },
                ),
                _ => (TaskState::Working, TaskResultState::Pending),
            }
        };
        let elapsed_ms = unix_timestamp_ms()
            .saturating_sub(view.deferred.created_at_ms)
            .max(0);
        let status_message = format!("state={} elapsed_ms={elapsed_ms}", state.as_str());
        debug_assert!(status_message.len() <= 256);
        Self {
            id: view.deferred.invocation_id,
            state,
            result,
            created_at_ms: view.deferred.created_at_ms,
            updated_at_ms: view.deferred.updated_at_ms,
            ttl_ms: view.deferred.advertised_ttl_ms(),
            poll_interval_ms,
            status_message,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use bazel_mcp_types::{
        BazelCommand, DeferredFailure, DeferredFailureKind, DeferredResultRecord, InvocationRecord,
        InvocationRequest,
    };

    use super::*;

    fn view(retrieval: DeferredRetrieval) -> DeferredResultView {
        let invocation = InvocationRecord::queued(InvocationRequest::new(
            PathBuf::from("/workspace"),
            BazelCommand::Build,
            vec!["//...".to_owned()],
        ));
        DeferredResultView {
            deferred: DeferredResultRecord::new(invocation.request.id, retrieval, 1_000, 61_000),
            invocation,
        }
    }

    #[test]
    fn execution_failure_is_failed_for_legacy_and_completed_for_extension() {
        let mut legacy = view(DeferredRetrieval::SeparateResult);
        legacy.invocation.state = InvocationState::Failed;
        legacy.deferred.failure = Some(DeferredFailure {
            kind: DeferredFailureKind::Execution,
            redacted_message: "bounded failure".to_owned(),
        });
        let legacy = TaskSnapshot::from_view(&legacy, 2_000);
        assert_eq!(legacy.state, TaskState::Failed);
        assert_eq!(
            legacy.result,
            TaskResultState::Invocation { tool_error: true }
        );

        let mut extension = view(DeferredRetrieval::InlineResult);
        extension.invocation.state = InvocationState::Failed;
        extension.deferred.failure = Some(DeferredFailure {
            kind: DeferredFailureKind::Execution,
            redacted_message: "bounded failure".to_owned(),
        });
        let extension = TaskSnapshot::from_view(&extension, 2_000);
        assert_eq!(extension.state, TaskState::Completed);
        assert_eq!(
            extension.result,
            TaskResultState::Invocation { tool_error: true }
        );
    }

    #[test]
    fn internal_failures_share_one_terminal_application_state() {
        for retrieval in [
            DeferredRetrieval::SeparateResult,
            DeferredRetrieval::InlineResult,
        ] {
            let mut view = view(retrieval);
            view.invocation.state = InvocationState::Failed;
            view.deferred.failure = Some(DeferredFailure {
                kind: DeferredFailureKind::Internal,
                redacted_message: "redacted".to_owned(),
            });
            let snapshot = TaskSnapshot::from_view(&view, 2_000);
            assert_eq!(snapshot.state, TaskState::Failed);
            assert_eq!(
                snapshot.result,
                TaskResultState::InternalFailure {
                    redacted_message: "redacted".to_owned()
                }
            );
        }
    }
}
