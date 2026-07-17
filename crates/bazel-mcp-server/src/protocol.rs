//! Negotiated MCP protocol adapters.

use std::collections::BTreeMap;

use rmcp::model::{
    CallToolResult, CustomResult, JsonObject, ProtocolVersion, ServerCapabilities, Task,
    TaskStatus, TaskSupport, Tool, ToolExecution, ToolsCapability,
};

use crate::{
    McpExecutionPolicy,
    tasks::{TaskSnapshot, TaskState},
};

pub(crate) const TASKS_EXTENSION: &str = "io.modelcontextprotocol/tasks";
pub(crate) const IMMEDIATE_RESPONSE: &str = "io.modelcontextprotocol/model-immediate-response";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProtocolFamily {
    Earlier,
    LegacyTasks,
    TasksExtension,
}

impl ProtocolFamily {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Earlier => "synchronous",
            Self::LegacyTasks => "legacy_tasks",
            Self::TasksExtension => "tasks_extension",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct SyncAdapter;

#[derive(Clone, Copy, Debug)]
pub(crate) struct LegacyTasksAdapter;

#[derive(Clone, Copy, Debug)]
pub(crate) struct TasksExtensionAdapter;

#[derive(Clone, Copy, Debug)]
pub(crate) enum ProtocolAdapter {
    Sync(SyncAdapter),
    Legacy(LegacyTasksAdapter),
    Extension(TasksExtensionAdapter),
}

impl ProtocolAdapter {
    #[must_use]
    pub(crate) fn negotiate(version: Option<&ProtocolVersion>) -> Self {
        match version {
            Some(version) if version == &ProtocolVersion::V_2025_11_25 => {
                Self::Legacy(LegacyTasksAdapter)
            }
            Some(version) if version.as_str() >= "2026-06-30" => {
                Self::Extension(TasksExtensionAdapter)
            }
            _ => Self::Sync(SyncAdapter),
        }
    }

    pub(crate) const fn family(self) -> ProtocolFamily {
        match self {
            Self::Sync(_) => ProtocolFamily::Earlier,
            Self::Legacy(_) => ProtocolFamily::LegacyTasks,
            Self::Extension(_) => ProtocolFamily::TasksExtension,
        }
    }

    #[must_use]
    pub(crate) fn capabilities(self, policy: McpExecutionPolicy) -> ServerCapabilities {
        let mut capabilities = ServerCapabilities::default();
        capabilities.tools = Some(ToolsCapability::default());
        match self {
            Self::Legacy(_) => {
                let tasks = if policy == McpExecutionPolicy::SyncOnly {
                    let mut tasks = rmcp::model::TasksCapability::default();
                    tasks.list = Some(JsonObject::new());
                    tasks.cancel = Some(JsonObject::new());
                    tasks
                } else {
                    rmcp::model::TasksCapability::server_default()
                };
                capabilities.tasks = Some(tasks);
            }
            Self::Extension(_) => {
                capabilities.extensions = Some(BTreeMap::from([(
                    TASKS_EXTENSION.to_owned(),
                    JsonObject::new(),
                )]));
            }
            Self::Sync(_) => {}
        }
        capabilities
    }

    pub(crate) fn adapt_tools(self, tools: &mut [Tool], policy: McpExecutionPolicy) {
        let Self::Legacy(_) = self else {
            return;
        };
        for tool in tools {
            if tool.name == "bazel.run" {
                tool.execution = match policy {
                    McpExecutionPolicy::Auto => {
                        Some(ToolExecution::new().with_task_support(TaskSupport::Optional))
                    }
                    McpExecutionPolicy::SyncOnly => None,
                    McpExecutionPolicy::TasksRequired => {
                        Some(ToolExecution::new().with_task_support(TaskSupport::Required))
                    }
                };
            }
        }
    }
}

impl LegacyTasksAdapter {
    #[must_use]
    pub(crate) fn task(self, snapshot: &TaskSnapshot) -> Task {
        Task::new(
            snapshot.id.to_string(),
            legacy_status(snapshot.state),
            format_timestamp(snapshot.created_at_ms),
            format_timestamp(snapshot.updated_at_ms),
        )
        .with_status_message(snapshot.status_message.clone())
        .with_ttl(snapshot.ttl_ms)
        .with_poll_interval(snapshot.poll_interval_ms)
    }
}

impl TasksExtensionAdapter {
    #[must_use]
    pub(crate) fn creation(self, snapshot: &TaskSnapshot) -> serde_json::Value {
        serde_json::json!({
            "resultType": "task",
            "taskId": snapshot.id.to_string(),
            "status": snapshot.state.as_str(),
            "createdAt": format_timestamp(snapshot.created_at_ms),
            "lastUpdatedAt": format_timestamp(snapshot.updated_at_ms),
            "ttlMs": snapshot.ttl_ms,
            "pollIntervalMs": snapshot.poll_interval_ms,
        })
    }

    pub(crate) fn complete(
        self,
        snapshot: &TaskSnapshot,
        result: Option<CallToolResult>,
        error: Option<&str>,
    ) -> Result<serde_json::Value, serde_json::Error> {
        let mut value = serde_json::json!({
            "resultType": "complete",
            "taskId": snapshot.id.to_string(),
            "status": snapshot.state.as_str(),
            "createdAt": format_timestamp(snapshot.created_at_ms),
            "lastUpdatedAt": format_timestamp(snapshot.updated_at_ms),
            "ttlMs": snapshot.ttl_ms,
            "pollIntervalMs": snapshot.poll_interval_ms,
        });
        if let Some(result) = result {
            value["result"] = serde_json::to_value(result)?;
        } else if let Some(message) = error {
            value["error"] = serde_json::json!({
                "code": rmcp::model::ErrorCode::INTERNAL_ERROR.0,
                "message": message,
            });
        }
        Ok(value)
    }

    #[must_use]
    pub(crate) fn acknowledge(self) -> CustomResult {
        CustomResult::new(serde_json::json!({"resultType": "complete"}))
    }
}

fn legacy_status(state: TaskState) -> TaskStatus {
    match state {
        TaskState::Working => TaskStatus::Working,
        TaskState::Completed => TaskStatus::Completed,
        TaskState::Failed => TaskStatus::Failed,
        TaskState::Cancelled => TaskStatus::Cancelled,
    }
}

fn format_timestamp(timestamp_ms: i64) -> String {
    let nanos = i128::from(timestamp_ms).saturating_mul(1_000_000);
    time::OffsetDateTime::from_unix_timestamp_nanos(nanos)
        .ok()
        .and_then(|timestamp| {
            timestamp
                .format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_owned())
}
