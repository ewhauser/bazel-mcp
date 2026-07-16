use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use bazel_mcp_types::{
    Artifact, CoverageFile, DeferredFailure, DeferredFailureKind, DeferredResultRecord,
    DeferredResultView, DeferredRetrieval, DeferredTerminalState, Diagnostic, InvocationId,
    InvocationMetrics, InvocationRecord, InvocationState, InvocationSummary, Page, PageRequest,
    QueryRow, Termination, TestResult,
};
use thiserror::Error;
use tokio::sync::Mutex;
use turso::{Connection, Database, Value, params};

use crate::{
    InvocationPaths,
    cursor::{DeferredCursor, InvocationCursor, OrdinalCursor},
};

const MIGRATION_1: &str = include_str!("../migrations/0001_initial.sql");
const MIGRATION_2: &str = include_str!("../migrations/0002_targets_coverage.sql");
const MIGRATION_3: &str = include_str!("../migrations/0003_canonical_arguments.sql");
const MIGRATION_4: &str = include_str!("../migrations/0004_cancellation_reason.sql");
const MIGRATION_5: &str = include_str!("../migrations/0005_deferred_results.sql");
const LATEST_SCHEMA_VERSION: i64 = 5;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("cache or database path is not valid UTF-8: {0}")]
    NonUtf8Path(PathBuf),
    #[error("invocation was not found: {0}")]
    NotFound(InvocationId),
    #[error("deferred result was not found or has expired: {0}")]
    DeferredNotFound(InvocationId),
    #[error("invalid pagination cursor")]
    InvalidCursor,
    #[error("unexpected database value in column {0}")]
    InvalidColumn(usize),
    #[error("database schema version {found} is newer than supported version {supported}")]
    UnsupportedSchemaVersion { found: i64, supported: i64 },
    #[error("database schema is inconsistent: {0}")]
    InconsistentSchema(String),
    #[error(transparent)]
    Database(#[from] turso::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    State(#[from] bazel_mcp_types::StateTransitionError),
    #[error(transparent)]
    Join(#[from] tokio::task::JoinError),
}

#[derive(Clone)]
pub struct Store {
    cache_root: PathBuf,
    database: Database,
    write_coordinator: Arc<Mutex<()>>,
}

impl Store {
    pub async fn open(cache_root: impl AsRef<Path>) -> Result<Self, StoreError> {
        let cache_root = cache_root.as_ref().to_owned();
        tokio::fs::create_dir_all(&cache_root).await?;
        set_private_root(&cache_root).await?;
        let database_path = cache_root.join("index.db");
        let path = database_path
            .to_str()
            .ok_or_else(|| StoreError::NonUtf8Path(database_path.clone()))?;
        let database = turso::Builder::new_local(path).build().await?;
        let store = Self {
            cache_root,
            database,
            write_coordinator: Arc::new(Mutex::new(())),
        };
        store.migrate().await?;
        store.recover_deletions().await?;
        store.recover_interrupted().await?;
        Ok(store)
    }

    #[must_use]
    pub fn paths_for(&self, record: &InvocationRecord) -> InvocationPaths {
        InvocationPaths::new(
            &self.cache_root,
            &record.request.workspace,
            record.request.id,
        )
    }

    pub async fn create_invocation(
        &self,
        record: &InvocationRecord,
    ) -> Result<InvocationPaths, StoreError> {
        self.create_invocation_with_deferred(record, None).await
    }

    /// Atomically accept an invocation and its optional deferred-result handle.
    pub async fn create_invocation_with_deferred(
        &self,
        record: &InvocationRecord,
        deferred: Option<&DeferredResultRecord>,
    ) -> Result<InvocationPaths, StoreError> {
        let paths = self.paths_for(record);
        paths.create().await?;
        let result = async {
            paths.write_request(record).await?;

            let _guard = self.write_coordinator.lock().await;
            let mut connection = self.database.connect()?;
            let transaction = connection.transaction().await?;
            transaction
                .execute(
                    "INSERT INTO invocations (
                        id, workspace, command, state, requested_at_ms,
                        request_json, metrics_json
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        record.request.id.to_string(),
                        record.request.workspace.to_string_lossy().into_owned(),
                        record.request.command.as_str(),
                        state_name(record.state),
                        record.request.requested_at_ms,
                        serde_json::to_string(&record.request)?,
                        serde_json::to_string(&record.metrics)?,
                    ],
                )
                .await?;
            if let Some(deferred) = deferred {
                transaction
                    .execute(
                        "INSERT INTO deferred_results (
                            invocation_id, retrieval_kind, created_at_ms, updated_at_ms,
                            expires_at_ms, cancellation_requested_at_ms, terminal_override,
                            failure_kind, failure_message
                         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                        params![
                            deferred.invocation_id.to_string(),
                            deferred.retrieval.as_str(),
                            deferred.created_at_ms,
                            deferred.updated_at_ms,
                            deferred.expires_at_ms,
                            option_i64(deferred.cancellation_requested_at_ms),
                            deferred
                                .terminal_override
                                .map_or(Value::Null, |value| Value::Text(value.as_str().into())),
                            deferred.failure.as_ref().map_or(Value::Null, |failure| {
                                Value::Text(failure.kind.as_str().into())
                            }),
                            deferred.failure.as_ref().map_or(Value::Null, |failure| {
                                Value::Text(failure.redacted_message.clone())
                            }),
                        ],
                    )
                    .await?;
            }
            transaction.commit().await?;
            Ok::<_, StoreError>(())
        }
        .await;
        if let Err(error) = result {
            let _ = tokio::fs::remove_dir_all(&paths.directory).await;
            return Err(error);
        }
        Ok(paths)
    }

    pub async fn get_deferred_result(
        &self,
        id: InvocationId,
        retrieval: DeferredRetrieval,
        now_ms: i64,
    ) -> Result<DeferredResultView, StoreError> {
        let invocation = self.get_invocation(id).await?;
        let connection = self.database.connect()?;
        let mut rows = connection
            .query(
                "SELECT retrieval_kind, created_at_ms, updated_at_ms, expires_at_ms,
                        cancellation_requested_at_ms, terminal_override,
                        failure_kind, failure_message
                 FROM deferred_results
                 WHERE invocation_id = ?1 AND retrieval_kind = ?2",
                params![id.to_string(), retrieval.as_str()],
            )
            .await?;
        let Some(row) = rows.next().await? else {
            return Err(StoreError::DeferredNotFound(id));
        };
        let deferred = deferred_from_row(id, &row)?;
        drop(rows);
        if deferred.is_expired(now_ms, invocation.state.is_terminal()) {
            let deleted = connection
                .execute(
                    "DELETE FROM deferred_results WHERE invocation_id = ?1",
                    params![id.to_string()],
                )
                .await?;
            if deleted > 0 {
                tracing::info!(
                    target: "bazel_mcp::metrics",
                    metric = "deferred_invocations_expired_total",
                    increment = deleted,
                    "expired deferred invocation metadata during lookup"
                );
            }
            return Err(StoreError::DeferredNotFound(id));
        }
        Ok(DeferredResultView {
            deferred,
            invocation,
        })
    }

    pub async fn list_deferred_results(
        &self,
        retrieval: DeferredRetrieval,
        now_ms: i64,
        page: PageRequest,
    ) -> Result<Page<DeferredResultView>, StoreError> {
        let limit = page.limit.clamp(1, 200) as usize;
        let cursor = page
            .cursor
            .as_deref()
            .map(|value| DeferredCursor::decode_for(value, retrieval.as_str()))
            .transpose()?;
        let connection = self.database.connect()?;
        let query = "SELECT d.invocation_id, d.retrieval_kind, d.created_at_ms,
                            d.updated_at_ms, d.expires_at_ms,
                            d.cancellation_requested_at_ms, d.terminal_override,
                            d.failure_kind, d.failure_message,
                            i.request_json, i.state, i.started_at_ms, i.finished_at_ms,
                            i.termination_json, i.summary_json, i.metrics_json,
                            i.canonical_arguments_json, i.cancellation_reason
                     FROM deferred_results d
                     JOIN invocations i ON i.id = d.invocation_id
                     WHERE d.retrieval_kind = ?1
                       AND (i.state NOT IN ('succeeded', 'failed', 'cancelled', 'timed_out', 'interrupted')
                            OR d.expires_at_ms > ?2)
                       AND (?3 IS NULL OR d.created_at_ms < ?3
                            OR (d.created_at_ms = ?3 AND d.invocation_id < ?4))
                     ORDER BY d.created_at_ms DESC, d.invocation_id DESC
                     LIMIT ?5";
        let cursor_time = cursor
            .as_ref()
            .map_or(Value::Null, |value| Value::Integer(value.created_at_ms));
        let cursor_id = cursor
            .as_ref()
            .map_or_else(String::new, |value| value.id.clone());
        let mut rows = connection
            .query(
                query,
                params![
                    retrieval.as_str(),
                    now_ms,
                    cursor_time,
                    cursor_id,
                    i64::try_from(limit + 1).unwrap_or(i64::MAX),
                ],
            )
            .await?;
        let mut items = Vec::new();
        while let Some(row) = rows.next().await? {
            let id_text: String = row.get(0)?;
            let id = parse_invocation_id(&id_text, 0)?;
            let deferred = deferred_from_joined_row(id, &row)?;
            let invocation = record_from_joined_deferred_row(&row)?;
            items.push(DeferredResultView {
                deferred,
                invocation,
            });
        }
        let truncated = items.len() > limit;
        items.truncate(limit);
        let next_cursor = if truncated {
            items
                .last()
                .map(|view| {
                    DeferredCursor::new(
                        retrieval.as_str(),
                        view.deferred.created_at_ms,
                        view.deferred.invocation_id.to_string(),
                    )
                    .encode()
                })
                .transpose()?
        } else {
            None
        };
        Ok(Page {
            items,
            next_cursor,
            truncated,
        })
    }

    pub async fn record_deferred_cancellation(
        &self,
        id: InvocationId,
        requested_at_ms: i64,
    ) -> Result<(), StoreError> {
        let _guard = self.write_coordinator.lock().await;
        let connection = self.database.connect()?;
        let changed = connection
            .execute(
                "UPDATE deferred_results
                 SET cancellation_requested_at_ms = COALESCE(cancellation_requested_at_ms, ?2),
                     updated_at_ms = MAX(updated_at_ms, ?2)
                 WHERE invocation_id = ?1",
                params![id.to_string(), requested_at_ms],
            )
            .await?;
        if changed == 0 {
            return Err(StoreError::DeferredNotFound(id));
        }
        Ok(())
    }

    pub async fn set_deferred_terminal_override(
        &self,
        id: InvocationId,
        state: DeferredTerminalState,
        updated_at_ms: i64,
    ) -> Result<(), StoreError> {
        let _guard = self.write_coordinator.lock().await;
        let connection = self.database.connect()?;
        let changed = connection
            .execute(
                "UPDATE deferred_results
                 SET terminal_override = ?2, updated_at_ms = MAX(updated_at_ms, ?3)
                 WHERE invocation_id = ?1",
                params![id.to_string(), state.as_str(), updated_at_ms],
            )
            .await?;
        if changed == 0 {
            return Err(StoreError::DeferredNotFound(id));
        }
        Ok(())
    }

    pub async fn persist_deferred_failure(
        &self,
        id: InvocationId,
        failure: &DeferredFailure,
        updated_at_ms: i64,
    ) -> Result<(), StoreError> {
        let _guard = self.write_coordinator.lock().await;
        let connection = self.database.connect()?;
        let changed = connection
            .execute(
                "UPDATE deferred_results
                 SET failure_kind = ?2, failure_message = ?3,
                     updated_at_ms = MAX(updated_at_ms, ?4)
                 WHERE invocation_id = ?1",
                params![
                    id.to_string(),
                    failure.kind.as_str(),
                    failure.redacted_message.as_str(),
                    updated_at_ms,
                ],
            )
            .await?;
        if changed == 0 {
            return Err(StoreError::DeferredNotFound(id));
        }
        Ok(())
    }

    pub async fn extend_deferred_expiry(
        &self,
        id: InvocationId,
        minimum_expires_at_ms: i64,
        updated_at_ms: i64,
    ) -> Result<(), StoreError> {
        let _guard = self.write_coordinator.lock().await;
        let connection = self.database.connect()?;
        let changed = connection
            .execute(
                "UPDATE deferred_results
                 SET expires_at_ms = MAX(expires_at_ms, ?2),
                     updated_at_ms = MAX(updated_at_ms, ?3)
                 WHERE invocation_id = ?1",
                params![id.to_string(), minimum_expires_at_ms, updated_at_ms],
            )
            .await?;
        if changed == 0 {
            return Err(StoreError::DeferredNotFound(id));
        }
        Ok(())
    }

    pub async fn delete_expired_deferred_results(&self, now_ms: i64) -> Result<usize, StoreError> {
        let _guard = self.write_coordinator.lock().await;
        let connection = self.database.connect()?;
        let deleted = connection
            .execute(
                "DELETE FROM deferred_results
                 WHERE expires_at_ms <= ?1
                   AND invocation_id IN (
                       SELECT id FROM invocations
                       WHERE state IN ('succeeded', 'failed', 'cancelled', 'timed_out', 'interrupted')
                   )",
                params![now_ms],
            )
            .await?;
        if deleted > 0 {
            tracing::info!(
                target: "bazel_mcp::metrics",
                metric = "deferred_invocations_expired_total",
                increment = deleted,
                "expired deferred invocation metadata during retention"
            );
        }
        Ok(usize::try_from(deleted).unwrap_or(usize::MAX))
    }

    pub async fn get_invocation(&self, id: InvocationId) -> Result<InvocationRecord, StoreError> {
        let connection = self.database.connect()?;
        let mut rows = connection
            .query(
                "SELECT request_json, state, started_at_ms, finished_at_ms,
                        termination_json, summary_json, metrics_json, canonical_arguments_json,
                        cancellation_reason
                 FROM invocations WHERE id = ?1",
                params![id.to_string()],
            )
            .await?;
        let row = rows.next().await?.ok_or(StoreError::NotFound(id))?;
        record_from_row(&row)
    }

    pub async fn transition(
        &self,
        id: InvocationId,
        next: InvocationState,
        termination: Option<Termination>,
        summary: Option<InvocationSummary>,
    ) -> Result<InvocationRecord, StoreError> {
        let _guard = self.write_coordinator.lock().await;
        let mut record = self.get_invocation(id).await?;
        record.transition(next)?;
        record.termination = termination;
        record.summary = summary;

        let termination_json = optional_json(&record.termination)?;
        let stored_summary = record.summary.as_ref().map(compact_summary);
        let summary_json = optional_json(&stored_summary)?;
        let mut connection = self.database.connect()?;
        let transaction = connection.transaction().await?;
        transaction
            .execute(
                "UPDATE invocations
                 SET state = ?2, started_at_ms = ?3, finished_at_ms = ?4,
                     termination_json = ?5, summary_json = ?6, metrics_json = ?7
                 WHERE id = ?1",
                params![
                    id.to_string(),
                    state_name(record.state),
                    option_i64(record.started_at_ms),
                    option_i64(record.finished_at_ms),
                    termination_json,
                    summary_json,
                    serde_json::to_string(&record.metrics)?,
                ],
            )
            .await?;
        replace_normalized_summary(&transaction, id, record.summary.as_ref()).await?;
        if next.is_terminal() {
            let terminal_at_ms = record
                .finished_at_ms
                .unwrap_or_else(bazel_mcp_types::unix_timestamp_ms);
            transaction
                .execute(
                    "UPDATE deferred_results
                     SET expires_at_ms = MAX(
                             expires_at_ms,
                             ?2 + MAX(1, expires_at_ms - created_at_ms)
                         ),
                         updated_at_ms = MAX(updated_at_ms, ?2)
                     WHERE invocation_id = ?1",
                    params![id.to_string(), terminal_at_ms],
                )
                .await?;
        }
        transaction.commit().await?;
        self.paths_for(&record).write_metadata(&record).await?;
        Ok(record)
    }

    pub async fn update_metrics(
        &self,
        id: InvocationId,
        metrics: InvocationMetrics,
    ) -> Result<(), StoreError> {
        let _guard = self.write_coordinator.lock().await;
        let connection = self.database.connect()?;
        connection
            .execute(
                "UPDATE invocations SET metrics_json = ?2 WHERE id = ?1",
                params![id.to_string(), serde_json::to_string(&metrics)?],
            )
            .await?;
        Ok(())
    }

    pub async fn record_model_visible_result(
        &self,
        id: InvocationId,
        bytes: u64,
        inspection: bool,
    ) -> Result<(), StoreError> {
        let _guard = self.write_coordinator.lock().await;
        let mut record = self.get_invocation(id).await?;
        record.metrics.model_visible_bytes =
            record.metrics.model_visible_bytes.saturating_add(bytes);
        if inspection {
            record.metrics.inspect_calls = record.metrics.inspect_calls.saturating_add(1);
        }
        let connection = self.database.connect()?;
        connection
            .execute(
                "UPDATE invocations SET metrics_json = ?2 WHERE id = ?1",
                params![id.to_string(), serde_json::to_string(&record.metrics)?],
            )
            .await?;
        let paths = self.paths_for(&record);
        let mut metadata = paths.read_metadata().await?;
        metadata.metrics = record.metrics;
        paths.write_metadata(&metadata).await?;
        Ok(())
    }

    pub async fn record_progress_notifications(
        &self,
        id: InvocationId,
        count: u64,
    ) -> Result<(), StoreError> {
        let _guard = self.write_coordinator.lock().await;
        let mut record = self.get_invocation(id).await?;
        record.metrics.progress_notifications =
            record.metrics.progress_notifications.saturating_add(count);
        let connection = self.database.connect()?;
        connection
            .execute(
                "UPDATE invocations SET metrics_json = ?2 WHERE id = ?1",
                params![id.to_string(), serde_json::to_string(&record.metrics)?],
            )
            .await?;
        let paths = self.paths_for(&record);
        let mut metadata = paths.read_metadata().await?;
        metadata.metrics = record.metrics;
        paths.write_metadata(&metadata).await?;
        Ok(())
    }

    pub async fn update_canonical_arguments(
        &self,
        id: InvocationId,
        arguments: &[String],
    ) -> Result<(), StoreError> {
        let _guard = self.write_coordinator.lock().await;
        let connection = self.database.connect()?;
        connection
            .execute(
                "UPDATE invocations SET canonical_arguments_json = ?2 WHERE id = ?1",
                params![id.to_string(), serde_json::to_string(arguments)?],
            )
            .await?;
        Ok(())
    }

    pub async fn update_cancellation_reason(
        &self,
        id: InvocationId,
        reason: &str,
    ) -> Result<(), StoreError> {
        let _guard = self.write_coordinator.lock().await;
        let mut record = self.get_invocation(id).await?;
        record.cancellation_reason = Some(reason.to_owned());
        let connection = self.database.connect()?;
        connection
            .execute(
                "UPDATE invocations SET cancellation_reason = ?2 WHERE id = ?1",
                params![id.to_string(), reason],
            )
            .await?;
        let paths = self.paths_for(&record);
        let mut metadata = paths.read_metadata().await?;
        metadata.cancellation_reason = record.cancellation_reason;
        paths.write_metadata(&metadata).await?;
        Ok(())
    }

    pub async fn list_invocations(
        &self,
        workspace: Option<&Path>,
        page: PageRequest,
    ) -> Result<Page<InvocationRecord>, StoreError> {
        let limit = page.limit.clamp(1, 200) as usize;
        let workspace = workspace.map(|path| path.to_string_lossy().into_owned());
        let cursor = page
            .cursor
            .as_deref()
            .map(|value| InvocationCursor::decode_for(value, workspace.as_deref()))
            .transpose()?;
        let connection = self.database.connect()?;
        let mut rows = match (&workspace, &cursor) {
            (None, None) => {
                connection
                    .query(
                        "SELECT request_json, state, started_at_ms, finished_at_ms,
                            termination_json, summary_json, metrics_json, canonical_arguments_json,
                            cancellation_reason
                     FROM invocations ORDER BY requested_at_ms DESC, id DESC LIMIT ?1",
                        params![i64::try_from(limit + 1).unwrap_or(i64::MAX)],
                    )
                    .await?
            }
            (Some(workspace), None) => {
                connection
                    .query(
                        "SELECT request_json, state, started_at_ms, finished_at_ms,
                            termination_json, summary_json, metrics_json, canonical_arguments_json,
                            cancellation_reason
                     FROM invocations WHERE workspace = ?1
                     ORDER BY requested_at_ms DESC, id DESC LIMIT ?2",
                        params![
                            workspace.as_str(),
                            i64::try_from(limit + 1).unwrap_or(i64::MAX)
                        ],
                    )
                    .await?
            }
            (None, Some(cursor)) => {
                connection
                    .query(
                        "SELECT request_json, state, started_at_ms, finished_at_ms,
                            termination_json, summary_json, metrics_json, canonical_arguments_json,
                            cancellation_reason
                     FROM invocations
                     WHERE requested_at_ms < ?1 OR (requested_at_ms = ?1 AND id < ?2)
                     ORDER BY requested_at_ms DESC, id DESC LIMIT ?3",
                        params![
                            cursor.requested_at_ms,
                            cursor.id.as_str(),
                            i64::try_from(limit + 1).unwrap_or(i64::MAX)
                        ],
                    )
                    .await?
            }
            (Some(workspace), Some(cursor)) => {
                connection
                    .query(
                        "SELECT request_json, state, started_at_ms, finished_at_ms,
                            termination_json, summary_json, metrics_json, canonical_arguments_json,
                            cancellation_reason
                     FROM invocations
                     WHERE workspace = ?1 AND
                       (requested_at_ms < ?2 OR (requested_at_ms = ?2 AND id < ?3))
                     ORDER BY requested_at_ms DESC, id DESC LIMIT ?4",
                        params![
                            workspace.as_str(),
                            cursor.requested_at_ms,
                            cursor.id.as_str(),
                            i64::try_from(limit + 1).unwrap_or(i64::MAX)
                        ],
                    )
                    .await?
            }
        };

        let mut items = Vec::new();
        while let Some(row) = rows.next().await? {
            items.push(record_from_row(&row)?);
        }
        let truncated = items.len() > limit;
        items.truncate(limit);
        let next_cursor = if truncated {
            items
                .last()
                .map(|record| {
                    InvocationCursor::new(
                        workspace.as_deref(),
                        record.request.requested_at_ms,
                        record.request.id.to_string(),
                    )
                })
                .map(|cursor| cursor.encode())
                .transpose()?
        } else {
            None
        };
        Ok(Page {
            items,
            next_cursor,
            truncated,
        })
    }

    pub async fn enforce_retention(
        &self,
        maximum_age: Duration,
        maximum_bytes: u64,
    ) -> Result<usize, StoreError> {
        let now_ms = bazel_mcp_types::unix_timestamp_ms();
        self.delete_expired_deferred_results(now_ms).await?;
        let cutoff =
            now_ms.saturating_sub(i64::try_from(maximum_age.as_millis()).unwrap_or(i64::MAX));
        let connection = self.database.connect()?;
        let mut rows = connection
            .query(
                "SELECT request_json, state, started_at_ms, finished_at_ms,
                        termination_json, summary_json, metrics_json, canonical_arguments_json,
                        cancellation_reason
                 FROM invocations
                 WHERE state IN ('succeeded', 'failed', 'cancelled', 'timed_out', 'interrupted')
                 ORDER BY finished_at_ms ASC, id ASC",
                (),
            )
            .await?;
        let mut records = Vec::new();
        while let Some(row) = rows.next().await? {
            records.push(record_from_row(&row)?);
        }
        let mut selected = BTreeSet::new();
        for record in &records {
            if record
                .finished_at_ms
                .is_some_and(|finished| finished < cutoff)
            {
                selected.insert(record.request.id);
            }
        }
        let root = self.cache_root.clone();
        let mut current_bytes =
            tokio::task::spawn_blocking(move || directory_size(&root)).await??;
        for record in &records {
            if selected.contains(&record.request.id) {
                let directory = self.paths_for(record).directory;
                let invocation_bytes =
                    tokio::task::spawn_blocking(move || directory_size(&directory)).await??;
                current_bytes = current_bytes.saturating_sub(invocation_bytes);
            }
        }
        for record in &records {
            if current_bytes <= maximum_bytes {
                break;
            }
            if selected.insert(record.request.id) {
                let directory = self.paths_for(record).directory;
                let invocation_bytes =
                    tokio::task::spawn_blocking(move || directory_size(&directory)).await??;
                current_bytes = current_bytes.saturating_sub(invocation_bytes);
            }
        }
        let mut deleted = 0;
        for id in selected {
            if let Ok(record) = self.get_invocation(id).await {
                if self.deferred_protects_invocation(id, now_ms).await? {
                    self.prune_deferred_evidence(&record).await?;
                } else {
                    self.delete_terminal_invocation(&record).await?;
                }
                deleted += 1;
            }
        }
        Ok(deleted)
    }

    async fn deferred_protects_invocation(
        &self,
        id: InvocationId,
        now_ms: i64,
    ) -> Result<bool, StoreError> {
        let connection = self.database.connect()?;
        let mut rows = connection
            .query(
                "SELECT 1 FROM deferred_results
                 WHERE invocation_id = ?1 AND expires_at_ms > ?2",
                params![id.to_string(), now_ms],
            )
            .await?;
        Ok(rows.next().await?.is_some())
    }

    async fn prune_deferred_evidence(&self, record: &InvocationRecord) -> Result<(), StoreError> {
        if !record.state.is_terminal() {
            return Ok(());
        }
        let paths = self.paths_for(record);
        let tombstone = paths.directory.with_extension("deleting");
        if paths.directory.exists() {
            tokio::fs::rename(&paths.directory, &tombstone).await?;
        }
        let _guard = self.write_coordinator.lock().await;
        let mut connection = self.database.connect()?;
        let transaction = connection.transaction().await?;
        for table in [
            "diagnostics",
            "test_results",
            "query_rows",
            "artifacts",
            "target_results",
            "coverage_files",
        ] {
            transaction
                .execute(
                    &format!("DELETE FROM {table} WHERE invocation_id = ?1"),
                    params![record.request.id.to_string()],
                )
                .await?;
        }
        transaction.commit().await?;
        drop(_guard);
        if tombstone.exists() {
            tokio::fs::remove_dir_all(tombstone).await?;
        }
        Ok(())
    }

    async fn delete_terminal_invocation(
        &self,
        record: &InvocationRecord,
    ) -> Result<(), StoreError> {
        if !record.state.is_terminal() {
            return Ok(());
        }
        let paths = self.paths_for(record);
        let tombstone = paths.directory.with_extension("deleting");
        if paths.directory.exists() {
            tokio::fs::rename(&paths.directory, &tombstone).await?;
        }
        let _guard = self.write_coordinator.lock().await;
        let mut connection = self.database.connect()?;
        let transaction = connection.transaction().await?;
        transaction
            .execute(
                "DELETE FROM deferred_results WHERE invocation_id = ?1",
                params![record.request.id.to_string()],
            )
            .await?;
        for table in [
            "diagnostics",
            "test_results",
            "query_rows",
            "artifacts",
            "target_results",
            "coverage_files",
        ] {
            transaction
                .execute(
                    &format!("DELETE FROM {table} WHERE invocation_id = ?1"),
                    params![record.request.id.to_string()],
                )
                .await?;
        }
        transaction
            .execute(
                "DELETE FROM invocations WHERE id = ?1",
                params![record.request.id.to_string()],
            )
            .await?;
        transaction.commit().await?;
        drop(_guard);
        if tombstone.exists() {
            tokio::fs::remove_dir_all(tombstone).await?;
        }
        Ok(())
    }

    pub async fn replace_query_rows(
        &self,
        id: InvocationId,
        rows: &[QueryRow],
    ) -> Result<(), StoreError> {
        let _guard = self.write_coordinator.lock().await;
        let mut connection = self.database.connect()?;
        let transaction = connection.transaction().await?;
        transaction
            .execute(
                "DELETE FROM query_rows WHERE invocation_id = ?1",
                params![id.to_string()],
            )
            .await?;
        for row in rows {
            transaction
                .execute(
                    "INSERT INTO query_rows(invocation_id, ordinal, value) VALUES (?1, ?2, ?3)",
                    params![
                        id.to_string(),
                        i64::try_from(row.ordinal).unwrap_or(i64::MAX),
                        row.value.as_str()
                    ],
                )
                .await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    pub async fn append_query_rows(
        &self,
        id: InvocationId,
        rows: &[QueryRow],
    ) -> Result<(), StoreError> {
        let _guard = self.write_coordinator.lock().await;
        let mut connection = self.database.connect()?;
        let transaction = connection.transaction().await?;
        for row in rows {
            transaction
                .execute(
                    "INSERT INTO query_rows(invocation_id, ordinal, value) VALUES (?1, ?2, ?3)",
                    params![
                        id.to_string(),
                        i64::try_from(row.ordinal).unwrap_or(i64::MAX),
                        row.value.as_str()
                    ],
                )
                .await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    pub async fn replace_artifacts(
        &self,
        id: InvocationId,
        artifacts: &[Artifact],
    ) -> Result<(), StoreError> {
        let _guard = self.write_coordinator.lock().await;
        let mut connection = self.database.connect()?;
        let transaction = connection.transaction().await?;
        transaction
            .execute(
                "DELETE FROM artifacts WHERE invocation_id = ?1",
                params![id.to_string()],
            )
            .await?;
        for (ordinal, artifact) in artifacts.iter().enumerate() {
            transaction
                .execute(
                    "INSERT INTO artifacts(invocation_id, ordinal, name, uri, record_json)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        id.to_string(),
                        i64::try_from(ordinal).unwrap_or(i64::MAX),
                        artifact.name.as_str(),
                        artifact.uri.as_str(),
                        serde_json::to_string(artifact)?
                    ],
                )
                .await?;
        }
        transaction.commit().await?;
        let record = self.get_invocation(id).await?;
        self.paths_for(&record).write_artifacts(artifacts).await?;
        Ok(())
    }

    pub async fn page_diagnostics(
        &self,
        id: InvocationId,
        filter: Option<&str>,
        page: PageRequest,
    ) -> Result<(Page<Diagnostic>, u64, u64), StoreError> {
        self.page_json_records(
            "diagnostics",
            id,
            filter,
            page,
            "message || ' ' || COALESCE(target, '')",
        )
        .await
    }

    pub async fn page_tests(
        &self,
        id: InvocationId,
        filter: Option<&str>,
        page: PageRequest,
    ) -> Result<(Page<TestResult>, u64, u64), StoreError> {
        self.page_json_records("test_results", id, filter, page, "label")
            .await
    }

    pub async fn page_artifacts(
        &self,
        id: InvocationId,
        filter: Option<&str>,
        page: PageRequest,
    ) -> Result<(Page<Artifact>, u64, u64), StoreError> {
        self.page_json_records("artifacts", id, filter, page, "name || ' ' || uri")
            .await
    }

    pub async fn page_coverage(
        &self,
        id: InvocationId,
        filter: Option<&str>,
        page: PageRequest,
    ) -> Result<(Page<CoverageFile>, u64, u64), StoreError> {
        self.page_json_records("coverage_files", id, filter, page, "path")
            .await
    }

    pub async fn page_query_rows(
        &self,
        id: InvocationId,
        filter: Option<&str>,
        page: PageRequest,
    ) -> Result<(Page<QueryRow>, u64, u64), StoreError> {
        let limit = page.limit.clamp(1, 100) as usize;
        let invocation_id = id.to_string();
        let after = page
            .cursor
            .as_deref()
            .map(|value| OrdinalCursor::decode_for(value, "query_rows", &invocation_id, filter))
            .transpose()?
            .map_or(-1, |cursor| cursor.ordinal);
        let connection = self.database.connect()?;
        let total = count_rows(&connection, "query_rows", id, None, "value").await?;
        let filtered = count_rows(&connection, "query_rows", id, filter, "value").await?;
        let mut rows = if let Some(filter) = filter {
            connection
                .query(
                    "SELECT ordinal, value FROM query_rows
                     WHERE invocation_id = ?1 AND ordinal > ?2 AND instr(value, ?3) > 0
                     ORDER BY ordinal LIMIT ?4",
                    params![
                        id.to_string(),
                        after,
                        filter,
                        i64::try_from(limit + 1).unwrap_or(i64::MAX)
                    ],
                )
                .await?
        } else {
            connection
                .query(
                    "SELECT ordinal, value FROM query_rows
                     WHERE invocation_id = ?1 AND ordinal > ?2
                     ORDER BY ordinal LIMIT ?3",
                    params![
                        id.to_string(),
                        after,
                        i64::try_from(limit + 1).unwrap_or(i64::MAX)
                    ],
                )
                .await?
        };
        let mut items = Vec::new();
        while let Some(row) = rows.next().await? {
            let ordinal: i64 = row.get(0)?;
            let value: String = row.get(1)?;
            items.push(QueryRow {
                ordinal: u64::try_from(ordinal).map_err(|_| StoreError::InvalidColumn(0))?,
                value,
            });
        }
        let truncated = items.len() > limit;
        items.truncate(limit);
        let next_cursor = if truncated {
            items
                .last()
                .map(|row| {
                    OrdinalCursor::new(
                        "query_rows",
                        &invocation_id,
                        filter,
                        i64::try_from(row.ordinal).unwrap_or(i64::MAX),
                    )
                })
                .map(|cursor| cursor.encode())
                .transpose()?
        } else {
            None
        };
        Ok((
            Page {
                items,
                next_cursor,
                truncated,
            },
            total,
            filtered,
        ))
    }

    async fn page_json_records<T: serde::de::DeserializeOwned>(
        &self,
        table: &str,
        id: InvocationId,
        filter: Option<&str>,
        page: PageRequest,
        filter_expression: &str,
    ) -> Result<(Page<T>, u64, u64), StoreError> {
        let allowed = matches!(
            table,
            "diagnostics" | "test_results" | "artifacts" | "coverage_files"
        );
        if !allowed {
            return Err(StoreError::InvalidCursor);
        }
        let limit = page.limit.clamp(1, 100) as usize;
        let invocation_id = id.to_string();
        let after = page
            .cursor
            .as_deref()
            .map(|value| OrdinalCursor::decode_for(value, table, &invocation_id, filter))
            .transpose()?
            .map_or(-1, |cursor| cursor.ordinal);
        let connection = self.database.connect()?;
        let total = count_rows(&connection, table, id, None, filter_expression).await?;
        let filtered = count_rows(&connection, table, id, filter, filter_expression).await?;
        let sql = if filter.is_some() {
            format!(
                "SELECT ordinal, record_json FROM {table}
                 WHERE invocation_id = ?1 AND ordinal > ?2 AND instr({filter_expression}, ?3) > 0
                 ORDER BY ordinal LIMIT ?4"
            )
        } else {
            format!(
                "SELECT ordinal, record_json FROM {table}
                 WHERE invocation_id = ?1 AND ordinal > ?2 ORDER BY ordinal LIMIT ?3"
            )
        };
        let mut rows = if let Some(filter) = filter {
            connection
                .query(
                    &sql,
                    params![
                        id.to_string(),
                        after,
                        filter,
                        i64::try_from(limit + 1).unwrap_or(i64::MAX)
                    ],
                )
                .await?
        } else {
            connection
                .query(
                    &sql,
                    params![
                        id.to_string(),
                        after,
                        i64::try_from(limit + 1).unwrap_or(i64::MAX)
                    ],
                )
                .await?
        };
        let mut items = Vec::new();
        let mut ordinals = Vec::new();
        while let Some(row) = rows.next().await? {
            ordinals.push(row.get::<i64>(0)?);
            let json: String = row.get(1)?;
            items.push(serde_json::from_str(&json)?);
        }
        let truncated = items.len() > limit;
        items.truncate(limit);
        let next_cursor = if truncated {
            Some(OrdinalCursor::new(table, &invocation_id, filter, ordinals[limit - 1]).encode()?)
        } else {
            None
        };
        Ok((
            Page {
                items,
                next_cursor,
                truncated,
            },
            total,
            filtered,
        ))
    }

    async fn migrate(&self) -> Result<(), StoreError> {
        let _guard = self.write_coordinator.lock().await;
        let connection = self.database.connect()?;
        if let Some(found) = recorded_schema_version(&connection).await?
            && found > LATEST_SCHEMA_VERSION
        {
            return Err(StoreError::UnsupportedSchemaVersion {
                found,
                supported: LATEST_SCHEMA_VERSION,
            });
        }
        // Migration 1 creates the ledger itself, so its idempotent DDL and
        // ledger write are always attempted in the same transaction.
        apply_migration(&connection, 1, MIGRATION_1).await?;
        for (version, sql) in [
            (2_i64, MIGRATION_2),
            (3_i64, MIGRATION_3),
            (4_i64, MIGRATION_4),
            (5_i64, MIGRATION_5),
        ] {
            if !migration_applied(&connection, version).await? {
                apply_migration(&connection, version, sql).await?;
            }
        }
        validate_schema(&connection).await?;
        Ok(())
    }

    async fn recover_interrupted(&self) -> Result<(), StoreError> {
        let _guard = self.write_coordinator.lock().await;
        let mut connection = self.database.connect()?;
        let mut rows = connection
            .query(
                "SELECT request_json, state, started_at_ms, finished_at_ms,
                        termination_json, summary_json, metrics_json, canonical_arguments_json,
                        cancellation_reason
                 FROM invocations WHERE state IN ('queued', 'starting', 'running')",
                (),
            )
            .await?;
        let mut interrupted = Vec::new();
        while let Some(row) = rows.next().await? {
            interrupted.push(record_from_row(&row)?);
        }
        drop(rows);
        let finished_at_ms = bazel_mcp_types::unix_timestamp_ms();
        let transaction = connection.transaction().await?;
        let mut orphaned_deferred = 0_u64;
        for mut record in interrupted {
            record.state = InvocationState::Interrupted;
            record.finished_at_ms = Some(finished_at_ms);
            record.termination = Some(Termination::Interrupted);
            record.summary = Some(InvocationSummary {
                success: false,
                headline: "Bazel invocation was interrupted by server restart".to_owned(),
                truncated: true,
                inspect_hint: Some("log".to_owned()),
                ..InvocationSummary::default()
            });
            transaction
                .execute(
                    "UPDATE invocations
                     SET state = 'interrupted', finished_at_ms = ?2,
                         termination_json = ?3, summary_json = ?4
                     WHERE id = ?1",
                    params![
                        record.request.id.to_string(),
                        finished_at_ms,
                        serde_json::to_string(&Termination::Interrupted)?,
                        serde_json::to_string(record.summary.as_ref().expect("summary set"))?,
                    ],
                )
                .await?;
            orphaned_deferred = orphaned_deferred.saturating_add(
                transaction
                    .execute(
                        "UPDATE deferred_results
                         SET expires_at_ms = MAX(
                                 expires_at_ms,
                                 ?2 + MAX(1, expires_at_ms - created_at_ms)
                             ),
                             updated_at_ms = MAX(updated_at_ms, ?2)
                         WHERE invocation_id = ?1",
                        params![record.request.id.to_string(), finished_at_ms],
                    )
                    .await?,
            );
            let paths = self.paths_for(&record);
            if paths.directory.is_dir() {
                paths.write_metadata(&record).await?;
            }
        }
        transaction.commit().await?;
        if orphaned_deferred > 0 {
            tracing::info!(
                target: "bazel_mcp::metrics",
                metric = "orphaned_deferred_work_total",
                increment = orphaned_deferred,
                "recovered orphaned deferred invocation metadata"
            );
        }
        Ok(())
    }

    async fn recover_deletions(&self) -> Result<(), StoreError> {
        let _guard = self.write_coordinator.lock().await;
        let root = self.cache_root.clone();
        let tombstones = tokio::task::spawn_blocking(move || find_tombstones(&root)).await??;
        let connection = self.database.connect()?;
        for tombstone in tombstones {
            let Some(id) = tombstone
                .file_stem()
                .and_then(std::ffi::OsStr::to_str)
                .map(str::to_owned)
            else {
                continue;
            };
            let mut rows = connection
                .query("SELECT 1 FROM invocations WHERE id = ?1", params![id])
                .await?;
            let indexed = rows.next().await?.is_some();
            drop(rows);
            let original = tombstone.with_extension("");
            if indexed && !original.exists() {
                tokio::fs::rename(tombstone, original).await?;
            } else {
                tokio::fs::remove_dir_all(tombstone).await?;
            }
        }
        Ok(())
    }
}

async fn migration_applied(connection: &Connection, version: i64) -> Result<bool, StoreError> {
    let mut rows = connection
        .query(
            "SELECT 1 FROM schema_migrations WHERE version = ?1",
            params![version],
        )
        .await?;
    Ok(rows.next().await?.is_some())
}

async fn recorded_schema_version(connection: &Connection) -> Result<Option<i64>, StoreError> {
    let mut rows = connection
        .query(
            "SELECT COUNT(*) FROM sqlite_schema
             WHERE type = 'table' AND name = 'schema_migrations'",
            (),
        )
        .await?;
    let exists = rows
        .next()
        .await?
        .ok_or(StoreError::InvalidColumn(0))?
        .get::<i64>(0)?
        > 0;
    drop(rows);
    if !exists {
        return Ok(None);
    }
    let mut rows = connection
        .query("SELECT MAX(version) FROM schema_migrations", ())
        .await?;
    let row = rows.next().await?.ok_or(StoreError::InvalidColumn(0))?;
    match row.get_value(0)? {
        Value::Null => Ok(None),
        Value::Integer(version) => Ok(Some(version)),
        _ => Err(StoreError::InvalidColumn(0)),
    }
}

async fn validate_schema(connection: &Connection) -> Result<(), StoreError> {
    for (table, columns) in [
        (
            "invocations",
            &[
                "id",
                "request_json",
                "metrics_json",
                "canonical_arguments_json",
                "cancellation_reason",
            ][..],
        ),
        ("diagnostics", &["invocation_id", "record_json"]),
        ("test_results", &["invocation_id", "record_json"]),
        ("query_rows", &["invocation_id", "value"]),
        ("artifacts", &["invocation_id", "record_json"]),
        ("target_results", &["invocation_id", "record_json"]),
        ("coverage_files", &["invocation_id", "record_json"]),
        (
            "deferred_results",
            &[
                "invocation_id",
                "retrieval_kind",
                "created_at_ms",
                "updated_at_ms",
                "expires_at_ms",
                "cancellation_requested_at_ms",
                "terminal_override",
                "failure_kind",
                "failure_message",
            ],
        ),
    ] {
        let mut rows = connection
            .query(&format!("PRAGMA table_info({table})"), ())
            .await?;
        let mut found = BTreeSet::new();
        while let Some(row) = rows.next().await? {
            found.insert(row.get::<String>(1)?);
        }
        for column in columns {
            if !found.contains(*column) {
                return Err(StoreError::InconsistentSchema(format!(
                    "table {table} is missing required column {column}"
                )));
            }
        }
    }
    Ok(())
}

async fn apply_migration(
    connection: &Connection,
    version: i64,
    sql: &str,
) -> Result<(), StoreError> {
    let applied_at_ms = bazel_mcp_types::unix_timestamp_ms();
    connection
        .execute_batch(&format!(
            "BEGIN;{sql}\nINSERT OR IGNORE INTO schema_migrations(version, applied_at_ms) \
             VALUES ({version}, {applied_at_ms});COMMIT;"
        ))
        .await?;
    Ok(())
}

fn directory_size(path: &Path) -> Result<u64, std::io::Error> {
    if !path.exists() {
        return Ok(0);
    }
    let mut size = 0_u64;
    let mut pending = vec![path.to_owned()];
    while let Some(path) = pending.pop() {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                pending.push(entry.path());
            } else {
                let metadata = if file_type.is_symlink() {
                    std::fs::symlink_metadata(entry.path())?
                } else {
                    entry.metadata()?
                };
                size = size.saturating_add(metadata.len());
            }
        }
    }
    Ok(size)
}

fn find_tombstones(path: &Path) -> Result<Vec<PathBuf>, std::io::Error> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut tombstones = Vec::new();
    let mut pending = vec![path.to_owned()];
    while let Some(path) = pending.pop() {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let path = entry.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "deleting")
            {
                tombstones.push(path);
            } else {
                pending.push(path);
            }
        }
    }
    Ok(tombstones)
}

async fn count_rows(
    connection: &turso::Connection,
    table: &str,
    id: InvocationId,
    filter: Option<&str>,
    filter_expression: &str,
) -> Result<u64, StoreError> {
    let sql = if filter.is_some() {
        format!(
            "SELECT COUNT(*) FROM {table} WHERE invocation_id = ?1 AND instr({filter_expression}, ?2) > 0"
        )
    } else {
        format!("SELECT COUNT(*) FROM {table} WHERE invocation_id = ?1")
    };
    let mut rows = if let Some(filter) = filter {
        connection
            .query(&sql, params![id.to_string(), filter])
            .await?
    } else {
        connection.query(&sql, params![id.to_string()]).await?
    };
    let row = rows.next().await?.ok_or(StoreError::InvalidColumn(0))?;
    let count: i64 = row.get(0)?;
    u64::try_from(count).map_err(|_| StoreError::InvalidColumn(0))
}

async fn replace_normalized_summary(
    connection: &turso::Connection,
    id: InvocationId,
    summary: Option<&InvocationSummary>,
) -> Result<(), StoreError> {
    connection
        .execute(
            "DELETE FROM diagnostics WHERE invocation_id = ?1",
            params![id.to_string()],
        )
        .await?;
    connection
        .execute(
            "DELETE FROM test_results WHERE invocation_id = ?1",
            params![id.to_string()],
        )
        .await?;
    connection
        .execute(
            "DELETE FROM target_results WHERE invocation_id = ?1",
            params![id.to_string()],
        )
        .await?;
    connection
        .execute(
            "DELETE FROM coverage_files WHERE invocation_id = ?1",
            params![id.to_string()],
        )
        .await?;
    let Some(summary) = summary else {
        return Ok(());
    };
    for (ordinal, diagnostic) in summary.diagnostics.iter().enumerate() {
        connection
            .execute(
                "INSERT INTO diagnostics(invocation_id, ordinal, severity, category, message, target, record_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    id.to_string(),
                    i64::try_from(ordinal).unwrap_or(i64::MAX),
                    format!("{:?}", diagnostic.severity).to_ascii_lowercase(),
                    format!("{:?}", diagnostic.category).to_ascii_lowercase(),
                    diagnostic.message.as_str(),
                    diagnostic.target.clone().map_or(Value::Null, Value::Text),
                    serde_json::to_string(diagnostic)?,
                ],
            )
            .await?;
    }
    for (ordinal, test) in summary.tests.iter().enumerate() {
        connection
            .execute(
                "INSERT INTO test_results(invocation_id, ordinal, label, status, record_json)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    id.to_string(),
                    i64::try_from(ordinal).unwrap_or(i64::MAX),
                    test.label.as_str(),
                    format!("{:?}", test.status).to_ascii_lowercase(),
                    serde_json::to_string(test)?,
                ],
            )
            .await?;
    }
    for (ordinal, target) in summary.targets.iter().enumerate() {
        connection
            .execute(
                "INSERT INTO target_results(invocation_id, ordinal, label, success, record_json)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    id.to_string(),
                    i64::try_from(ordinal).unwrap_or(i64::MAX),
                    target.label.as_str(),
                    i64::from(target.success),
                    serde_json::to_string(target)?,
                ],
            )
            .await?;
    }
    if let Some(coverage) = &summary.coverage {
        for (ordinal, file) in coverage.files.iter().enumerate() {
            connection
                .execute(
                    "INSERT INTO coverage_files(invocation_id, ordinal, path, record_json)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        id.to_string(),
                        i64::try_from(ordinal).unwrap_or(i64::MAX),
                        file.path.as_str(),
                        serde_json::to_string(file)?,
                    ],
                )
                .await?;
        }
    }
    Ok(())
}

fn record_from_row(row: &turso::Row) -> Result<InvocationRecord, StoreError> {
    let request_json: String = row.get(0)?;
    let mut record = InvocationRecord::queued(serde_json::from_str(&request_json)?);
    let state: String = row.get(1)?;
    record.state = parse_state(&state).ok_or(StoreError::InvalidColumn(1))?;
    record.started_at_ms = nullable_i64(row, 2)?;
    record.finished_at_ms = nullable_i64(row, 3)?;
    record.termination = nullable_json(row, 4)?;
    record.summary = nullable_json(row, 5)?;
    let metrics: String = row.get(6)?;
    record.metrics = serde_json::from_str(&metrics)?;
    record.canonical_arguments = nullable_json(row, 7)?;
    record.cancellation_reason = match row.get_value(8)? {
        Value::Null => None,
        Value::Text(value) => Some(value),
        _ => return Err(StoreError::InvalidColumn(8)),
    };
    Ok(record)
}

fn record_from_joined_deferred_row(row: &turso::Row) -> Result<InvocationRecord, StoreError> {
    let request_json: String = row.get(9)?;
    let mut record = InvocationRecord::queued(serde_json::from_str(&request_json)?);
    let state: String = row.get(10)?;
    record.state = parse_state(&state).ok_or(StoreError::InvalidColumn(10))?;
    record.started_at_ms = nullable_i64(row, 11)?;
    record.finished_at_ms = nullable_i64(row, 12)?;
    record.termination = nullable_json(row, 13)?;
    record.summary = nullable_json(row, 14)?;
    let metrics: String = row.get(15)?;
    record.metrics = serde_json::from_str(&metrics)?;
    record.canonical_arguments = nullable_json(row, 16)?;
    record.cancellation_reason = nullable_text(row, 17)?;
    Ok(record)
}

fn deferred_from_row(
    id: InvocationId,
    row: &turso::Row,
) -> Result<DeferredResultRecord, StoreError> {
    deferred_from_columns(id, row, 0)
}

fn deferred_from_joined_row(
    id: InvocationId,
    row: &turso::Row,
) -> Result<DeferredResultRecord, StoreError> {
    deferred_from_columns(id, row, 1)
}

fn deferred_from_columns(
    id: InvocationId,
    row: &turso::Row,
    offset: usize,
) -> Result<DeferredResultRecord, StoreError> {
    let retrieval: String = row.get(offset)?;
    let retrieval =
        DeferredRetrieval::parse(&retrieval).ok_or(StoreError::InvalidColumn(offset))?;
    let terminal_override = nullable_text(row, offset + 5)?
        .map(|value| {
            DeferredTerminalState::parse(&value).ok_or(StoreError::InvalidColumn(offset + 5))
        })
        .transpose()?;
    let failure_kind = nullable_text(row, offset + 6)?;
    let failure_message = nullable_text(row, offset + 7)?;
    let failure = match (failure_kind, failure_message) {
        (None, None) => None,
        (Some(kind), Some(redacted_message)) => Some(DeferredFailure {
            kind: DeferredFailureKind::parse(&kind).ok_or(StoreError::InvalidColumn(offset + 6))?,
            redacted_message,
        }),
        _ => return Err(StoreError::InvalidColumn(offset + 6)),
    };
    Ok(DeferredResultRecord {
        invocation_id: id,
        retrieval,
        created_at_ms: row.get(offset + 1)?,
        updated_at_ms: row.get(offset + 2)?,
        expires_at_ms: row.get(offset + 3)?,
        cancellation_requested_at_ms: nullable_i64(row, offset + 4)?,
        terminal_override,
        failure,
    })
}

fn parse_invocation_id(value: &str, column: usize) -> Result<InvocationId, StoreError> {
    serde_json::from_value(serde_json::Value::String(value.to_owned()))
        .map_err(|_| StoreError::InvalidColumn(column))
}

fn nullable_i64(row: &turso::Row, index: usize) -> Result<Option<i64>, StoreError> {
    match row.get_value(index)? {
        Value::Null => Ok(None),
        Value::Integer(value) => Ok(Some(value)),
        _ => Err(StoreError::InvalidColumn(index)),
    }
}

fn nullable_text(row: &turso::Row, index: usize) -> Result<Option<String>, StoreError> {
    match row.get_value(index)? {
        Value::Null => Ok(None),
        Value::Text(value) => Ok(Some(value)),
        _ => Err(StoreError::InvalidColumn(index)),
    }
}

fn nullable_json<T: serde::de::DeserializeOwned>(
    row: &turso::Row,
    index: usize,
) -> Result<Option<T>, StoreError> {
    match row.get_value(index)? {
        Value::Null => Ok(None),
        Value::Text(value) => Ok(Some(serde_json::from_str(&value)?)),
        _ => Err(StoreError::InvalidColumn(index)),
    }
}

fn optional_json<T: serde::Serialize>(value: &Option<T>) -> Result<Value, StoreError> {
    match value {
        Some(value) => Ok(Value::Text(serde_json::to_string(value)?)),
        None => Ok(Value::Null),
    }
}

fn compact_summary(summary: &InvocationSummary) -> InvocationSummary {
    let mut compact = summary.clone();
    compact.targets.clear();
    compact.tests.clear();
    if let Some(coverage) = &mut compact.coverage {
        coverage.files.clear();
    }
    compact
}

fn option_i64(value: Option<i64>) -> Value {
    value.map_or(Value::Null, Value::Integer)
}

const fn state_name(state: InvocationState) -> &'static str {
    match state {
        InvocationState::Queued => "queued",
        InvocationState::Starting => "starting",
        InvocationState::Running => "running",
        InvocationState::Succeeded => "succeeded",
        InvocationState::Failed => "failed",
        InvocationState::Cancelled => "cancelled",
        InvocationState::TimedOut => "timed_out",
        InvocationState::Interrupted => "interrupted",
    }
}

fn parse_state(value: &str) -> Option<InvocationState> {
    Some(match value {
        "queued" => InvocationState::Queued,
        "starting" => InvocationState::Starting,
        "running" => InvocationState::Running,
        "succeeded" => InvocationState::Succeeded,
        "failed" => InvocationState::Failed,
        "cancelled" => InvocationState::Cancelled,
        "timed_out" => InvocationState::TimedOut,
        "interrupted" => InvocationState::Interrupted,
        _ => return None,
    })
}

#[cfg(unix)]
async fn set_private_root(path: &Path) -> Result<(), StoreError> {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).await?;
    Ok(())
}

#[cfg(not(unix))]
async fn set_private_root(_path: &Path) -> Result<(), StoreError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use bazel_mcp_types::{
        Artifact, ArtifactKind, BazelCommand, CoverageFile, CoverageSummary, Diagnostic,
        DiagnosticCategory, InvocationRequest, QueryRow, Severity, TargetResult, TestResult,
        TestStatus,
    };
    use tempfile::tempdir;

    use super::*;

    #[tokio::test]
    async fn persists_and_transitions_an_invocation() {
        let root = tempdir().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let request = InvocationRequest::new(
            PathBuf::from("/tmp/workspace"),
            BazelCommand::Build,
            vec!["//...".into()],
        );
        let id = request.id;
        store
            .create_invocation(&InvocationRecord::queued(request))
            .await
            .unwrap();
        store
            .transition(id, InvocationState::Starting, None, None)
            .await
            .unwrap();
        store
            .transition(id, InvocationState::Running, None, None)
            .await
            .unwrap();
        let record = store.get_invocation(id).await.unwrap();
        assert_eq!(record.state, InvocationState::Running);
    }

    #[tokio::test]
    async fn deferred_results_are_atomic_typed_paged_and_expiry_aware() {
        let root = tempdir().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let now = 10_000_i64;
        let mut ids = Vec::new();
        for offset in 0..3_i64 {
            let mut request = InvocationRequest::new(
                PathBuf::from("/tmp/workspace"),
                BazelCommand::Build,
                vec![format!("//:task-{offset}")],
            );
            request.requested_at_ms = now + offset;
            let id = request.id;
            let record = InvocationRecord::queued(request);
            let deferred = DeferredResultRecord::new(
                id,
                DeferredRetrieval::SeparateResult,
                now + offset,
                now + offset + 1_000,
            );
            store
                .create_invocation_with_deferred(&record, Some(&deferred))
                .await
                .unwrap();
            ids.push(id);
        }

        let first = store
            .get_deferred_result(ids[0], DeferredRetrieval::SeparateResult, now)
            .await
            .unwrap();
        assert_eq!(first.invocation.request.id, ids[0]);
        assert!(matches!(
            store
                .get_deferred_result(ids[0], DeferredRetrieval::InlineResult, now)
                .await,
            Err(StoreError::DeferredNotFound(_))
        ));

        let page = store
            .list_deferred_results(
                DeferredRetrieval::SeparateResult,
                now,
                PageRequest {
                    cursor: None,
                    limit: 2,
                },
            )
            .await
            .unwrap();
        assert_eq!(page.items.len(), 2);
        assert!(page.truncated);
        assert_eq!(page.items[0].deferred.invocation_id, ids[2]);
        let second_page = store
            .list_deferred_results(
                DeferredRetrieval::SeparateResult,
                now,
                PageRequest {
                    cursor: page.next_cursor,
                    limit: 2,
                },
            )
            .await
            .unwrap();
        assert_eq!(second_page.items.len(), 1);
        assert_eq!(second_page.items[0].deferred.invocation_id, ids[0]);

        store
            .record_deferred_cancellation(ids[0], now + 10)
            .await
            .unwrap();
        store
            .persist_deferred_failure(
                ids[0],
                &DeferredFailure {
                    kind: DeferredFailureKind::Execution,
                    redacted_message: "bounded failure".to_owned(),
                },
                now + 11,
            )
            .await
            .unwrap();
        store
            .set_deferred_terminal_override(ids[0], DeferredTerminalState::Cancelled, now + 12)
            .await
            .unwrap();
        let updated = store
            .get_deferred_result(ids[0], DeferredRetrieval::SeparateResult, now + 20)
            .await
            .unwrap();
        assert_eq!(
            updated.deferred.cancellation_requested_at_ms,
            Some(now + 10)
        );
        assert_eq!(
            updated.deferred.terminal_override,
            Some(DeferredTerminalState::Cancelled)
        );
        assert_eq!(
            updated.deferred.failure.unwrap().redacted_message,
            "bounded failure"
        );

        store
            .transition(ids[1], InvocationState::Starting, None, None)
            .await
            .unwrap();
        store
            .transition(ids[1], InvocationState::Running, None, None)
            .await
            .unwrap();
        store
            .transition(
                ids[1],
                InvocationState::Succeeded,
                Some(Termination::Exit { code: 0 }),
                Some(InvocationSummary {
                    success: true,
                    headline: "done".to_owned(),
                    ..Default::default()
                }),
            )
            .await
            .unwrap();
        let terminal = store
            .get_deferred_result(
                ids[1],
                DeferredRetrieval::SeparateResult,
                bazel_mcp_types::unix_timestamp_ms(),
            )
            .await
            .unwrap();
        assert!(
            terminal.deferred.expires_at_ms
                >= terminal
                    .invocation
                    .finished_at_ms
                    .unwrap()
                    .saturating_add(1_000)
        );

        // A queued task is never expired, even after its initial window.
        assert!(
            store
                .get_deferred_result(ids[2], DeferredRetrieval::SeparateResult, i64::MAX,)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn duplicate_invocation_creation_never_overwrites_existing_evidence() {
        let root = tempdir().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let request = InvocationRequest::new(
            PathBuf::from("/tmp/workspace"),
            BazelCommand::Build,
            vec!["//original".into()],
        );
        let id = request.id;
        let original = InvocationRecord::queued(request.clone());
        let paths = store.create_invocation(&original).await.unwrap();
        let original_request = tokio::fs::read(&paths.request).await.unwrap();

        let mut duplicate_request = request;
        duplicate_request.arguments = vec!["//replacement".into()];
        let duplicate = InvocationRecord::queued(duplicate_request);
        assert!(store.create_invocation(&duplicate).await.is_err());

        assert_eq!(
            tokio::fs::read(&paths.request).await.unwrap(),
            original_request
        );
        assert_eq!(
            store.get_invocation(id).await.unwrap().request,
            original.request
        );
    }

    #[tokio::test]
    async fn opening_store_recovers_orphaned_running_invocations() {
        let root = tempdir().unwrap();
        let request = InvocationRequest::new(
            PathBuf::from("/tmp/workspace"),
            BazelCommand::Test,
            vec!["//...".into()],
        );
        let id = request.id;
        {
            let store = Store::open(root.path()).await.unwrap();
            store
                .create_invocation(&InvocationRecord::queued(request))
                .await
                .unwrap();
            store
                .transition(id, InvocationState::Starting, None, None)
                .await
                .unwrap();
            store
                .transition(id, InvocationState::Running, None, None)
                .await
                .unwrap();
        }
        let reopened = Store::open(root.path()).await.unwrap();
        let record = reopened.get_invocation(id).await.unwrap();
        assert_eq!(record.state, InvocationState::Interrupted);
        let metadata: InvocationRecord = serde_json::from_slice(
            &tokio::fs::read(reopened.paths_for(&record).metadata)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(metadata.state, InvocationState::Interrupted);
        assert_eq!(metadata.termination, Some(Termination::Interrupted));
    }

    #[tokio::test]
    async fn opening_store_completes_or_rolls_back_interrupted_deletions() {
        let root = tempdir().unwrap();
        let request = InvocationRequest::new(
            PathBuf::from("/tmp/workspace"),
            BazelCommand::Build,
            vec!["//...".into()],
        );
        let id = request.id;
        let invocation_paths;
        {
            let store = Store::open(root.path()).await.unwrap();
            invocation_paths = store
                .create_invocation(&InvocationRecord::queued(request))
                .await
                .unwrap();
        }
        let indexed_tombstone = invocation_paths.directory.with_extension("deleting");
        tokio::fs::rename(&invocation_paths.directory, &indexed_tombstone)
            .await
            .unwrap();
        let stale_tombstone = root
            .path()
            .join("workspaces/orphan/invocations/orphan.deleting");
        tokio::fs::create_dir_all(&stale_tombstone).await.unwrap();

        let reopened = Store::open(root.path()).await.unwrap();
        let record = reopened.get_invocation(id).await.unwrap();
        assert_eq!(record.state, InvocationState::Interrupted);
        assert!(invocation_paths.directory.is_dir());
        assert!(!indexed_tombstone.exists());
        assert!(!stale_tombstone.exists());
        let metadata: InvocationRecord =
            serde_json::from_slice(&tokio::fs::read(invocation_paths.metadata).await.unwrap())
                .unwrap();
        assert_eq!(metadata.state, InvocationState::Interrupted);
    }

    #[tokio::test]
    async fn upgrades_a_v1_database_and_records_every_migration() {
        let root = tempdir().unwrap();
        let database_path = root.path().join("index.db");
        let database = turso::Builder::new_local(database_path.to_str().unwrap())
            .build()
            .await
            .unwrap();
        let connection = database.connect().unwrap();
        connection.execute_batch(MIGRATION_1).await.unwrap();
        connection
            .execute(
                "INSERT INTO schema_migrations(version, applied_at_ms) VALUES (1, 1)",
                (),
            )
            .await
            .unwrap();
        drop(connection);
        drop(database);

        let store = Store::open(root.path()).await.unwrap();
        let connection = store.database.connect().unwrap();
        let mut rows = connection
            .query(
                "SELECT COUNT(*), MIN(version), MAX(version) FROM schema_migrations",
                (),
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        assert_eq!(row.get::<i64>(0).unwrap(), 5);
        assert_eq!(row.get::<i64>(1).unwrap(), 1);
        assert_eq!(row.get::<i64>(2).unwrap(), 5);

        let mut rows = connection
            .query("PRAGMA table_info(invocations)", ())
            .await
            .unwrap();
        let mut columns = BTreeSet::new();
        while let Some(row) = rows.next().await.unwrap() {
            columns.insert(row.get::<String>(1).unwrap());
        }
        assert!(columns.contains("canonical_arguments_json"));
        assert!(columns.contains("cancellation_reason"));
    }

    #[tokio::test]
    async fn rejects_corrupt_newer_and_inconsistent_databases_without_overwriting_them() {
        let corrupt = tempdir().unwrap();
        tokio::fs::write(corrupt.path().join("index.db"), b"not a database")
            .await
            .unwrap();
        assert!(Store::open(corrupt.path()).await.is_err());
        assert_eq!(
            tokio::fs::read(corrupt.path().join("index.db"))
                .await
                .unwrap(),
            b"not a database"
        );

        let newer = tempdir().unwrap();
        let database_path = newer.path().join("index.db");
        let database = turso::Builder::new_local(database_path.to_str().unwrap())
            .build()
            .await
            .unwrap();
        let connection = database.connect().unwrap();
        connection.execute_batch(MIGRATION_1).await.unwrap();
        connection
            .execute(
                "INSERT INTO schema_migrations(version, applied_at_ms) VALUES (999, 1)",
                (),
            )
            .await
            .unwrap();
        drop(connection);
        drop(database);
        assert!(matches!(
            Store::open(newer.path()).await,
            Err(StoreError::UnsupportedSchemaVersion { found: 999, .. })
        ));

        let inconsistent = tempdir().unwrap();
        let database_path = inconsistent.path().join("index.db");
        let database = turso::Builder::new_local(database_path.to_str().unwrap())
            .build()
            .await
            .unwrap();
        let connection = database.connect().unwrap();
        connection.execute_batch(MIGRATION_1).await.unwrap();
        for version in 1..=LATEST_SCHEMA_VERSION {
            connection
                .execute(
                    "INSERT OR IGNORE INTO schema_migrations(version, applied_at_ms) VALUES (?1, 1)",
                    params![version],
                )
                .await
                .unwrap();
        }
        drop(connection);
        drop(database);
        assert!(matches!(
            Store::open(inconsistent.path()).await,
            Err(StoreError::InconsistentSchema(_))
        ));
    }

    #[tokio::test]
    async fn rejects_a_cache_root_that_is_not_a_writable_directory() {
        let root = tempdir().unwrap();
        let regular_file = root.path().join("cache-root");
        tokio::fs::write(&regular_file, b"occupied").await.unwrap();
        assert!(matches!(
            Store::open(&regular_file).await,
            Err(StoreError::Io(_))
        ));
        assert_eq!(tokio::fs::read(&regular_file).await.unwrap(), b"occupied");
    }

    #[tokio::test]
    async fn retention_never_evicts_a_long_running_invocation_to_meet_quota() {
        let root = tempdir().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let mut request = InvocationRequest::new(
            PathBuf::from("/tmp/workspace"),
            BazelCommand::Build,
            vec!["//:long-running".into()],
        );
        request.requested_at_ms = request.requested_at_ms.saturating_sub(8 * 60 * 60 * 1000);
        let id = request.id;
        store
            .create_invocation(&InvocationRecord::queued(request))
            .await
            .unwrap();
        store
            .transition(id, InvocationState::Starting, None, None)
            .await
            .unwrap();
        store
            .transition(id, InvocationState::Running, None, None)
            .await
            .unwrap();

        assert_eq!(
            store
                .enforce_retention(Duration::from_secs(60), 1)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            store.get_invocation(id).await.unwrap().state,
            InvocationState::Running
        );
    }

    #[tokio::test]
    async fn normalized_views_filter_and_page_with_opaque_cursors() {
        let root = tempdir().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let request = InvocationRequest::new(
            PathBuf::from("/tmp/workspace"),
            BazelCommand::Query,
            vec!["//...".into()],
        );
        let id = request.id;
        store
            .create_invocation(&InvocationRecord::queued(request))
            .await
            .unwrap();
        store
            .transition(id, InvocationState::Starting, None, None)
            .await
            .unwrap();
        store
            .transition(id, InvocationState::Running, None, None)
            .await
            .unwrap();
        let summary = InvocationSummary {
            diagnostics: vec![Diagnostic {
                severity: Severity::Error,
                category: DiagnosticCategory::Loading,
                message: "no such target //missing:one".into(),
                location: None,
                target: Some("//missing:one".into()),
                action: None,
                repetition_count: 1,
            }],
            ..InvocationSummary::default()
        };
        store
            .transition(
                id,
                InvocationState::Failed,
                Some(Termination::Exit { code: 1 }),
                Some(summary),
            )
            .await
            .unwrap();
        store
            .replace_query_rows(
                id,
                &[
                    QueryRow {
                        ordinal: 0,
                        value: "//a:first".into(),
                    },
                    QueryRow {
                        ordinal: 1,
                        value: "//b:second".into(),
                    },
                ],
            )
            .await
            .unwrap();
        store
            .replace_artifacts(
                id,
                &[Artifact {
                    name: "out.txt".into(),
                    kind: ArtifactKind::File,
                    uri: "file:///tmp/out.txt".into(),
                    size_bytes: Some(1),
                    locally_available: true,
                }],
            )
            .await
            .unwrap();
        let (first, total, filtered) = store
            .page_query_rows(
                id,
                None,
                PageRequest {
                    cursor: None,
                    limit: 1,
                },
            )
            .await
            .unwrap();
        assert_eq!((total, filtered), (2, 2));
        assert!(first.next_cursor.is_some());
        let cursor = first.next_cursor.clone().unwrap();
        assert!(
            store
                .page_query_rows(
                    id,
                    Some("second"),
                    PageRequest {
                        cursor: Some(cursor.clone()),
                        limit: 1,
                    },
                )
                .await
                .is_err()
        );
        assert!(
            store
                .page_artifacts(
                    id,
                    None,
                    PageRequest {
                        cursor: Some(cursor),
                        limit: 1,
                    },
                )
                .await
                .is_err()
        );
        let (second, _, _) = store
            .page_query_rows(
                id,
                Some("second"),
                PageRequest {
                    cursor: None,
                    limit: 10,
                },
            )
            .await
            .unwrap();
        assert_eq!(second.items[0].ordinal, 1);
        assert_eq!(
            store
                .page_artifacts(id, Some("out"), PageRequest::default())
                .await
                .unwrap()
                .0
                .items
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn metadata_updates_preserve_full_summary_and_artifacts_file() {
        let root = tempdir().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let request = InvocationRequest::new(
            PathBuf::from("/tmp/workspace"),
            BazelCommand::Test,
            vec!["//pkg:test".into()],
        );
        let id = request.id;
        let paths = store
            .create_invocation(&InvocationRecord::queued(request))
            .await
            .unwrap();
        store
            .transition(id, InvocationState::Starting, None, None)
            .await
            .unwrap();
        store
            .transition(id, InvocationState::Running, None, None)
            .await
            .unwrap();
        let summary = InvocationSummary {
            targets: vec![TargetResult {
                label: "//pkg:test".into(),
                success: false,
            }],
            tests: vec![TestResult {
                label: "//pkg:test".into(),
                status: TestStatus::Failed,
                duration_ms: Some(1),
                attempts: 1,
                shard: None,
                cases: Vec::new(),
                log_uri: None,
            }],
            coverage: Some(CoverageSummary {
                lines_found: 1,
                lines_hit: 0,
                coverage_percent: 0.0,
                files: vec![CoverageFile {
                    path: "src/lib.rs".into(),
                    lines_found: 1,
                    lines_hit: 0,
                    coverage_percent: 0.0,
                }],
            }),
            ..InvocationSummary::default()
        };
        store
            .transition(
                id,
                InvocationState::Failed,
                Some(Termination::Exit { code: 1 }),
                Some(summary),
            )
            .await
            .unwrap();
        store
            .record_model_visible_result(id, 100, false)
            .await
            .unwrap();

        let metadata: InvocationRecord =
            serde_json::from_slice(&tokio::fs::read(&paths.metadata).await.unwrap()).unwrap();
        assert_eq!(metadata.summary.as_ref().unwrap().targets.len(), 1);
        assert_eq!(metadata.summary.as_ref().unwrap().tests.len(), 1);
        assert_eq!(
            metadata
                .summary
                .as_ref()
                .unwrap()
                .coverage
                .as_ref()
                .unwrap()
                .files
                .len(),
            1
        );

        let artifact = Artifact {
            name: "out.txt".into(),
            kind: ArtifactKind::File,
            uri: "file:///tmp/out.txt".into(),
            size_bytes: Some(1),
            locally_available: true,
        };
        store
            .replace_artifacts(id, std::slice::from_ref(&artifact))
            .await
            .unwrap();
        let artifacts: Vec<Artifact> =
            serde_json::from_slice(&tokio::fs::read(&paths.artifacts).await.unwrap()).unwrap();
        assert_eq!(artifacts, vec![artifact]);
    }

    #[tokio::test]
    async fn retention_accounts_for_age_selected_bytes_before_quota_eviction() {
        let root = tempdir().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let mut ids = Vec::new();
        let mut directories = Vec::new();
        for target in ["//old", "//new"] {
            let request = InvocationRequest::new(
                PathBuf::from("/tmp/workspace"),
                BazelCommand::Build,
                vec![target.into()],
            );
            let id = request.id;
            let paths = store
                .create_invocation(&InvocationRecord::queued(request))
                .await
                .unwrap();
            store
                .transition(id, InvocationState::Starting, None, None)
                .await
                .unwrap();
            store
                .transition(id, InvocationState::Running, None, None)
                .await
                .unwrap();
            store
                .transition(
                    id,
                    InvocationState::Succeeded,
                    Some(Termination::Exit { code: 0 }),
                    Some(InvocationSummary::default()),
                )
                .await
                .unwrap();
            tokio::fs::write(&paths.stdout, vec![b'x'; 4 * 1024])
                .await
                .unwrap();
            ids.push(id);
            directories.push(paths.directory);
        }
        let connection = store.database.connect().unwrap();
        connection
            .execute(
                "UPDATE invocations SET finished_at_ms = 0 WHERE id = ?1",
                params![ids[0].to_string()],
            )
            .await
            .unwrap();
        let root_size = directory_size(root.path()).unwrap();
        let old_size = directory_size(&directories[0]).unwrap();

        let deleted = store
            .enforce_retention(Duration::from_secs(24 * 60 * 60), root_size - old_size)
            .await
            .unwrap();

        assert_eq!(deleted, 1);
        assert!(matches!(
            store.get_invocation(ids[0]).await,
            Err(StoreError::NotFound(_))
        ));
        assert!(store.get_invocation(ids[1]).await.is_ok());
    }
}
