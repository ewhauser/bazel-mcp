use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use bazel_mcp_types::{
    Artifact, CoverageFile, Diagnostic, InvocationId, InvocationMetrics, InvocationRecord,
    InvocationState, InvocationSummary, Page, PageRequest, QueryRow, Termination, TestResult,
};
use thiserror::Error;
use tokio::sync::Mutex;
use turso::{Connection, Database, Value, params};

use crate::{
    InvocationPaths,
    cursor::{InvocationCursor, OrdinalCursor},
};

const MIGRATION_1: &str = include_str!("../migrations/0001_initial.sql");
const MIGRATION_2: &str = include_str!("../migrations/0002_targets_coverage.sql");
const MIGRATION_3: &str = include_str!("../migrations/0003_canonical_arguments.sql");
const MIGRATION_4: &str = include_str!("../migrations/0004_cancellation_reason.sql");

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("cache or database path is not valid UTF-8: {0}")]
    NonUtf8Path(PathBuf),
    #[error("invocation was not found: {0}")]
    NotFound(InvocationId),
    #[error("invalid pagination cursor")]
    InvalidCursor,
    #[error("unexpected database value in column {0}")]
    InvalidColumn(usize),
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
        let cutoff = bazel_mcp_types::unix_timestamp_ms()
            .saturating_sub(i64::try_from(maximum_age.as_millis()).unwrap_or(i64::MAX));
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
                self.delete_terminal_invocation(&record).await?;
                deleted += 1;
            }
        }
        Ok(deleted)
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
        // Migration 1 creates the ledger itself, so its idempotent DDL and
        // ledger write are always attempted in the same transaction.
        apply_migration(&connection, 1, MIGRATION_1).await?;
        for (version, sql) in [
            (2_i64, MIGRATION_2),
            (3_i64, MIGRATION_3),
            (4_i64, MIGRATION_4),
        ] {
            if !migration_applied(&connection, version).await? {
                apply_migration(&connection, version, sql).await?;
            }
        }
        Ok(())
    }

    async fn recover_interrupted(&self) -> Result<(), StoreError> {
        let _guard = self.write_coordinator.lock().await;
        let connection = self.database.connect()?;
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
        let termination = serde_json::to_string(&Termination::Interrupted)?;
        connection
            .execute(
                "UPDATE invocations SET state = 'interrupted', finished_at_ms = ?1,
                    termination_json = ?2 WHERE state IN ('queued', 'starting', 'running')",
                params![finished_at_ms, termination],
            )
            .await?;
        for mut record in interrupted {
            record.state = InvocationState::Interrupted;
            record.finished_at_ms = Some(finished_at_ms);
            record.termination = Some(Termination::Interrupted);
            let paths = self.paths_for(&record);
            if paths.directory.is_dir() {
                paths.write_metadata(&record).await?;
            }
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

fn nullable_i64(row: &turso::Row, index: usize) -> Result<Option<i64>, StoreError> {
    match row.get_value(index)? {
        Value::Null => Ok(None),
        Value::Integer(value) => Ok(Some(value)),
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
        assert_eq!(row.get::<i64>(0).unwrap(), 4);
        assert_eq!(row.get::<i64>(1).unwrap(), 1);
        assert_eq!(row.get::<i64>(2).unwrap(), 4);

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
