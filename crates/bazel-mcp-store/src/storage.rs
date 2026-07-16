use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions},
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use bazel_mcp_types::{
    Artifact, CoverageFile, DeferredFailure, DeferredResultRecord, DeferredResultView,
    DeferredRetrieval, DeferredTerminalState, Diagnostic, InvocationId, InvocationMetrics,
    InvocationRecord, InvocationState, InvocationSummary, Page, PageRequest, QueryRow, Termination,
    TestResult,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;
use tokio::sync::Mutex;

use crate::{
    InvocationPaths,
    cursor::{DeferredCursor, FileCursor, InvocationCursor, OrdinalCursor},
    files::{set_private_directory, set_private_file, write_json_atomic},
};

const RECORD_SCHEMA_VERSION: u32 = 1;
const QUERY_LINE_LIMIT: usize = 64 * 1024;
const GC_LOW_WATERMARK_PERCENT: u64 = 80;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("invocation was not found: {0}")]
    NotFound(InvocationId),
    #[error("deferred result was not found or has expired: {0}")]
    DeferredNotFound(InvocationId),
    #[error("invalid pagination cursor")]
    InvalidCursor,
    #[error("cache is already locked by another process: {0}")]
    Locked(PathBuf),
    #[error("unsupported invocation record schema {found} in {path}")]
    UnsupportedRecordSchema { found: u32, path: PathBuf },
    #[error("corrupt invocation record {path}: {message}")]
    CorruptRecord { path: PathBuf, message: String },
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
    inner: Arc<StoreInner>,
}

struct StoreInner {
    index: Mutex<Index>,
    _lock_file: File,
}

#[derive(Default)]
struct Index {
    entries: BTreeMap<InvocationId, IndexEntry>,
    by_requested: BTreeSet<(i64, InvocationId)>,
    by_workspace: BTreeMap<PathBuf, BTreeSet<(i64, InvocationId)>>,
    deferred_by_created: BTreeSet<(i64, InvocationId)>,
    terminal_by_finished: BTreeSet<(i64, InvocationId)>,
    retained_bytes: u64,
}

#[derive(Clone)]
struct IndexEntry {
    record: InvocationRecord,
    deferred: Option<DeferredResultRecord>,
    evidence_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DurableRecord {
    schema_version: u32,
    invocation: InvocationRecord,
    #[serde(default)]
    deferred: Option<DeferredResultRecord>,
    #[serde(default)]
    evidence_bytes: u64,
}

impl DurableRecord {
    fn index_entry(&self) -> IndexEntry {
        IndexEntry {
            record: compact_record(&self.invocation),
            deferred: self.deferred.clone(),
            evidence_bytes: self.evidence_bytes,
        }
    }
}

impl Store {
    pub async fn open(cache_root: impl AsRef<Path>) -> Result<Self, StoreError> {
        let cache_root = cache_root.as_ref().to_owned();
        tokio::fs::create_dir_all(&cache_root).await?;
        set_private_directory(&cache_root).await?;
        tokio::fs::create_dir_all(cache_root.join("invocations")).await?;
        tokio::fs::create_dir_all(cache_root.join("trash")).await?;
        set_private_directory(&cache_root.join("invocations")).await?;
        set_private_directory(&cache_root.join("trash")).await?;

        let lock_path = cache_root.join("LOCK");
        let lock_file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)?;
        set_private_file(&lock_path).await?;
        match lock_file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(StoreError::Locked(lock_path));
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        }

        recover_trash(&cache_root).await?;
        let mut index = load_index(&cache_root).await?;
        recover_interrupted(&cache_root, &mut index).await?;

        Ok(Self {
            cache_root,
            inner: Arc::new(StoreInner {
                index: Mutex::new(index),
                _lock_file: lock_file,
            }),
        })
    }

    #[must_use]
    pub fn paths_for(&self, record: &InvocationRecord) -> InvocationPaths {
        InvocationPaths::new(&self.cache_root, record.request.id)
    }

    fn paths_for_id(&self, id: InvocationId) -> InvocationPaths {
        InvocationPaths::new(&self.cache_root, id)
    }

    pub async fn create_invocation(
        &self,
        record: &InvocationRecord,
    ) -> Result<InvocationPaths, StoreError> {
        self.create_invocation_with_deferred(record, None).await
    }

    pub async fn create_invocation_with_deferred(
        &self,
        record: &InvocationRecord,
        deferred: Option<&DeferredResultRecord>,
    ) -> Result<InvocationPaths, StoreError> {
        let paths = self.paths_for(record);
        let mut index = self.inner.index.lock().await;
        if index.entries.contains_key(&record.request.id) {
            return Err(StoreError::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "invocation already exists",
            )));
        }
        paths.create().await?;
        let result = async {
            paths.write_request(record).await?;
            let mut durable = DurableRecord {
                schema_version: RECORD_SCHEMA_VERSION,
                invocation: record.clone(),
                deferred: deferred.cloned(),
                evidence_bytes: 0,
            };
            persist_durable(&paths, &mut durable).await?;
            Ok::<DurableRecord, StoreError>(durable)
        }
        .await;
        let durable = match result {
            Ok(durable) => durable,
            Err(error) => {
                let _ = tokio::fs::remove_dir_all(&paths.directory).await;
                return Err(error);
            }
        };
        insert_index_entry(&mut index, record.request.id, durable.index_entry());
        Ok(paths)
    }

    pub async fn get_deferred_result(
        &self,
        id: InvocationId,
        retrieval: DeferredRetrieval,
        now_ms: i64,
    ) -> Result<DeferredResultView, StoreError> {
        let mut index = self.inner.index.lock().await;
        let entry = index.entries.get(&id).ok_or(StoreError::NotFound(id))?;
        let Some(deferred) = entry.deferred.clone() else {
            return Err(StoreError::DeferredNotFound(id));
        };
        if deferred.retrieval != retrieval
            || deferred.is_expired(now_ms, entry.record.state.is_terminal())
        {
            if deferred.is_expired(now_ms, entry.record.state.is_terminal()) {
                let mut durable = read_durable(&self.paths_for_id(id).metadata).await?;
                durable.deferred = None;
                persist_durable(&self.paths_for_id(id), &mut durable).await?;
                replace_index_entry(&mut index, id, durable.index_entry());
            }
            return Err(StoreError::DeferredNotFound(id));
        }
        Ok(DeferredResultView {
            deferred,
            invocation: entry.record.clone(),
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
        let index = self.inner.index.lock().await;
        let mut items: Vec<_> = index
            .deferred_by_created
            .iter()
            .rev()
            .filter_map(|(_, id)| {
                let entry = index.entries.get(id)?;
                let deferred = entry.deferred.as_ref()?;
                (deferred.retrieval == retrieval
                    && !deferred.is_expired(now_ms, entry.record.state.is_terminal())
                    && cursor.as_ref().is_none_or(|cursor| {
                        deferred.created_at_ms < cursor.created_at_ms
                            || (deferred.created_at_ms == cursor.created_at_ms
                                && deferred.invocation_id.to_string() < cursor.id)
                    }))
                .then(|| DeferredResultView {
                    deferred: deferred.clone(),
                    invocation: entry.record.clone(),
                })
            })
            .take(limit + 1)
            .collect();
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
        self.mutate(id, |durable| {
            let deferred = durable
                .deferred
                .as_mut()
                .ok_or(StoreError::DeferredNotFound(id))?;
            if deferred.cancellation_requested_at_ms.is_none() {
                deferred.cancellation_requested_at_ms = Some(requested_at_ms);
            }
            deferred.updated_at_ms = deferred.updated_at_ms.max(requested_at_ms);
            Ok(())
        })
        .await
    }

    pub async fn set_deferred_terminal_override(
        &self,
        id: InvocationId,
        state: DeferredTerminalState,
        updated_at_ms: i64,
    ) -> Result<(), StoreError> {
        self.mutate(id, |durable| {
            let deferred = durable
                .deferred
                .as_mut()
                .ok_or(StoreError::DeferredNotFound(id))?;
            deferred.terminal_override = Some(state);
            deferred.updated_at_ms = deferred.updated_at_ms.max(updated_at_ms);
            Ok(())
        })
        .await
    }

    pub async fn persist_deferred_failure(
        &self,
        id: InvocationId,
        failure: &DeferredFailure,
        updated_at_ms: i64,
    ) -> Result<(), StoreError> {
        self.mutate(id, |durable| {
            let deferred = durable
                .deferred
                .as_mut()
                .ok_or(StoreError::DeferredNotFound(id))?;
            deferred.failure = Some(failure.clone());
            deferred.updated_at_ms = deferred.updated_at_ms.max(updated_at_ms);
            Ok(())
        })
        .await
    }

    pub async fn extend_deferred_expiry(
        &self,
        id: InvocationId,
        minimum_expires_at_ms: i64,
        updated_at_ms: i64,
    ) -> Result<(), StoreError> {
        self.mutate(id, |durable| {
            let deferred = durable
                .deferred
                .as_mut()
                .ok_or(StoreError::DeferredNotFound(id))?;
            deferred.expires_at_ms = deferred.expires_at_ms.max(minimum_expires_at_ms);
            deferred.updated_at_ms = deferred.updated_at_ms.max(updated_at_ms);
            Ok(())
        })
        .await
    }

    pub async fn delete_expired_deferred_results(&self, now_ms: i64) -> Result<usize, StoreError> {
        let mut index = self.inner.index.lock().await;
        let ids: Vec<_> = index
            .entries
            .iter()
            .filter_map(|(id, entry)| {
                entry
                    .deferred
                    .as_ref()
                    .is_some_and(|deferred| {
                        deferred.is_expired(now_ms, entry.record.state.is_terminal())
                    })
                    .then_some(*id)
            })
            .collect();
        for id in &ids {
            let paths = self.paths_for_id(*id);
            let mut durable = read_durable(&paths.metadata).await?;
            durable.deferred = None;
            persist_durable(&paths, &mut durable).await?;
            replace_index_entry(&mut index, *id, durable.index_entry());
        }
        Ok(ids.len())
    }

    pub async fn get_invocation(&self, id: InvocationId) -> Result<InvocationRecord, StoreError> {
        self.inner
            .index
            .lock()
            .await
            .entries
            .get(&id)
            .map(|entry| entry.record.clone())
            .ok_or(StoreError::NotFound(id))
    }

    pub async fn transition(
        &self,
        id: InvocationId,
        next: InvocationState,
        termination: Option<Termination>,
        summary: Option<InvocationSummary>,
    ) -> Result<InvocationRecord, StoreError> {
        let mut index = self.inner.index.lock().await;
        ensure_exists(&index, id)?;
        let paths = self.paths_for_id(id);
        let mut durable = match read_durable(&paths.metadata).await {
            Ok(durable) => durable,
            Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return transition_lost_evidence(&mut index, id, next, termination, summary);
            }
            Err(error) => return Err(error),
        };
        durable.invocation.transition(next)?;
        durable.invocation.termination = termination.clone();
        durable.invocation.summary = summary.clone();
        if next.is_terminal()
            && let Some(deferred) = durable.deferred.as_mut()
        {
            let terminal_at_ms = durable
                .invocation
                .finished_at_ms
                .unwrap_or_else(bazel_mcp_types::unix_timestamp_ms);
            deferred.extend_terminal_expiry(terminal_at_ms);
        }
        // The summary becomes durable before the terminal record is committed.
        if let Err(error) = paths.write_summary(&durable.invocation).await {
            if error_is_not_found(&error) {
                return transition_lost_evidence(&mut index, id, next, termination, summary);
            }
            return Err(error);
        }
        if let Err(error) = persist_durable(&paths, &mut durable).await {
            if error_is_not_found(&error) {
                return transition_lost_evidence(&mut index, id, next, termination, summary);
            }
            return Err(error);
        }
        let result = durable.invocation.clone();
        replace_index_entry(&mut index, id, durable.index_entry());
        Ok(result)
    }

    pub async fn update_metrics(
        &self,
        id: InvocationId,
        metrics: InvocationMetrics,
    ) -> Result<(), StoreError> {
        self.mutate(id, |durable| {
            durable.invocation.metrics = metrics;
            Ok(())
        })
        .await
    }

    pub async fn record_model_visible_result(
        &self,
        id: InvocationId,
        bytes: u64,
        inspection: bool,
    ) -> Result<(), StoreError> {
        self.mutate(id, |durable| {
            let metrics = &mut durable.invocation.metrics;
            metrics.model_visible_bytes = metrics.model_visible_bytes.saturating_add(bytes);
            if inspection {
                metrics.inspect_calls = metrics.inspect_calls.saturating_add(1);
            }
            Ok(())
        })
        .await
    }

    pub async fn record_progress_notifications(
        &self,
        id: InvocationId,
        count: u64,
    ) -> Result<(), StoreError> {
        self.mutate(id, |durable| {
            let metrics = &mut durable.invocation.metrics;
            metrics.progress_notifications = metrics.progress_notifications.saturating_add(count);
            Ok(())
        })
        .await
    }

    pub async fn update_canonical_arguments(
        &self,
        id: InvocationId,
        arguments: &[String],
    ) -> Result<(), StoreError> {
        self.mutate(id, |durable| {
            durable.invocation.canonical_arguments = Some(arguments.to_vec());
            Ok(())
        })
        .await
    }

    pub async fn update_cancellation_reason(
        &self,
        id: InvocationId,
        reason: &str,
    ) -> Result<(), StoreError> {
        self.mutate(id, |durable| {
            durable.invocation.cancellation_reason = Some(reason.to_owned());
            Ok(())
        })
        .await
    }

    pub async fn list_invocations(
        &self,
        workspace: Option<&Path>,
        page: PageRequest,
    ) -> Result<Page<InvocationRecord>, StoreError> {
        let limit = page.limit.clamp(1, 200) as usize;
        let workspace_text = workspace.map(|path| path.to_string_lossy().into_owned());
        let cursor = page
            .cursor
            .as_deref()
            .map(|value| InvocationCursor::decode_for(value, workspace_text.as_deref()))
            .transpose()?;
        let index = self.inner.index.lock().await;
        let collect = |ordered: &BTreeSet<(i64, InvocationId)>| {
            ordered
                .iter()
                .rev()
                .filter(|(requested_at_ms, id)| {
                    cursor.as_ref().is_none_or(|cursor| {
                        *requested_at_ms < cursor.requested_at_ms
                            || (*requested_at_ms == cursor.requested_at_ms
                                && id.to_string() < cursor.id)
                    })
                })
                .filter_map(|(_, id)| index.entries.get(id).map(|entry| entry.record.clone()))
                .take(limit + 1)
                .collect::<Vec<_>>()
        };
        let mut items = if let Some(workspace) = workspace {
            index
                .by_workspace
                .get(workspace)
                .map_or_else(Vec::new, collect)
        } else {
            collect(&index.by_requested)
        };
        let truncated = items.len() > limit;
        items.truncate(limit);
        let next_cursor = if truncated {
            items
                .last()
                .map(|record| {
                    InvocationCursor::new(
                        workspace_text.as_deref(),
                        record.request.requested_at_ms,
                        record.request.id.to_string(),
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

    pub async fn enforce_retention(
        &self,
        maximum_age: Duration,
        maximum_bytes: u64,
    ) -> Result<usize, StoreError> {
        let now_ms = bazel_mcp_types::unix_timestamp_ms();
        self.delete_expired_deferred_results(now_ms).await?;
        let cutoff =
            now_ms.saturating_sub(i64::try_from(maximum_age.as_millis()).unwrap_or(i64::MAX));
        let mut index = self.inner.index.lock().await;
        // Running evidence can grow without metadata commits. Refresh only the
        // bounded live set; terminal bytes remain commit-accounted, so normal
        // GC never walks the cache tree.
        let live_ids: Vec<_> = index
            .entries
            .iter()
            .filter_map(|(id, entry)| (!entry.record.state.is_terminal()).then_some(*id))
            .collect();
        for id in live_ids {
            let current = evidence_size(&self.paths_for_id(id)).await?;
            let previous = index.entries.get_mut(&id).map(|entry| {
                let previous = entry.evidence_bytes;
                entry.evidence_bytes = current;
                previous
            });
            if let Some(previous) = previous {
                index.retained_bytes = index.retained_bytes.saturating_sub(previous);
                index.retained_bytes = index.retained_bytes.saturating_add(current);
            }
        }
        let candidates: Vec<_> = index
            .terminal_by_finished
            .iter()
            .filter_map(|(finished, id)| {
                let entry = index.entries.get(id)?;
                Some((
                    *id,
                    *finished,
                    entry.deferred.as_ref().is_some_and(|deferred| {
                        !deferred.is_expired(now_ms, entry.record.state.is_terminal())
                    }),
                ))
            })
            .collect();

        let low_watermark = maximum_bytes
            .saturating_mul(GC_LOW_WATERMARK_PERCENT)
            .checked_div(100)
            .unwrap_or(0);
        let mut reclaimed = 0;
        let mut processed = BTreeSet::new();
        for (id, finished, protected) in &candidates {
            if *finished < cutoff {
                if self.reclaim_terminal(&mut index, *id, *protected).await? {
                    reclaimed += 1;
                }
                processed.insert(*id);
            }
        }
        if index.retained_bytes > maximum_bytes {
            for (id, _, protected) in candidates {
                if index.retained_bytes <= low_watermark {
                    break;
                }
                if processed.insert(id) && self.reclaim_terminal(&mut index, id, protected).await? {
                    reclaimed += 1;
                }
            }
        }
        Ok(reclaimed)
    }

    async fn reclaim_terminal(
        &self,
        index: &mut Index,
        id: InvocationId,
        deferred_protected: bool,
    ) -> Result<bool, StoreError> {
        if deferred_protected {
            let paths = self.paths_for_id(id);
            let before = index
                .entries
                .get(&id)
                .map_or(0, |entry| entry.evidence_bytes);
            let mut durable = read_durable(&paths.metadata).await?;
            stage_raw_evidence(&self.cache_root, id, &paths).await?;
            persist_durable(&paths, &mut durable).await?;
            let after = durable.evidence_bytes;
            replace_index_entry(index, id, durable.index_entry());
            finish_staged_evidence(&self.cache_root, id).await?;
            return Ok(after < before);
        }
        if let Some(trash) = rename_to_trash(&self.cache_root, id).await? {
            remove_index_entry(index, id);
            // Rename is the deletion commit. If unlinking fails, the index
            // stays removed and startup finishes this trash entry.
            tokio::fs::remove_dir_all(trash).await?;
            return Ok(true);
        }
        Ok(false)
    }

    pub async fn replace_artifacts(
        &self,
        id: InvocationId,
        artifacts: &[Artifact],
    ) -> Result<(), StoreError> {
        let mut index = self.inner.index.lock().await;
        ensure_exists(&index, id)?;
        let paths = self.paths_for_id(id);
        paths.write_artifacts(artifacts).await?;
        let mut durable = read_durable(&paths.metadata).await?;
        persist_durable(&paths, &mut durable).await?;
        replace_index_entry(&mut index, id, durable.index_entry());
        Ok(())
    }

    pub async fn page_diagnostics(
        &self,
        id: InvocationId,
        filter: Option<&str>,
        page: PageRequest,
    ) -> Result<(Page<Diagnostic>, u64, u64), StoreError> {
        let record = self.read_full_invocation(id).await?;
        let items = record
            .summary
            .map_or_else(Vec::new, |summary| summary.diagnostics);
        page_records("diagnostics", id, filter, page, items, |item| {
            format!(
                "{} {}",
                item.message,
                item.target.as_deref().unwrap_or_default()
            )
        })
    }

    pub async fn page_tests(
        &self,
        id: InvocationId,
        filter: Option<&str>,
        page: PageRequest,
    ) -> Result<(Page<TestResult>, u64, u64), StoreError> {
        let record = self.read_full_invocation(id).await?;
        let items = record
            .summary
            .map_or_else(Vec::new, |summary| summary.tests);
        page_records("test_results", id, filter, page, items, |item| {
            item.label.clone()
        })
    }

    pub async fn page_artifacts(
        &self,
        id: InvocationId,
        filter: Option<&str>,
        page: PageRequest,
    ) -> Result<(Page<Artifact>, u64, u64), StoreError> {
        self.ensure_invocation(id).await?;
        let path = self.paths_for_id(id).artifacts;
        let items: Vec<Artifact> = read_json_or_default(&path).await?;
        page_records("artifacts", id, filter, page, items, |item| {
            format!("{} {}", item.name, item.uri)
        })
    }

    pub async fn page_coverage(
        &self,
        id: InvocationId,
        filter: Option<&str>,
        page: PageRequest,
    ) -> Result<(Page<CoverageFile>, u64, u64), StoreError> {
        let record = self.read_full_invocation(id).await?;
        let items = record
            .summary
            .and_then(|summary| summary.coverage)
            .map_or_else(Vec::new, |coverage| coverage.files);
        page_records("coverage_files", id, filter, page, items, |item| {
            item.path.clone()
        })
    }

    pub async fn page_query_rows(
        &self,
        id: InvocationId,
        filter: Option<&str>,
        page: PageRequest,
    ) -> Result<(Page<QueryRow>, u64, u64), StoreError> {
        self.page_query_rows_mapped(id, filter, page, str::to_owned)
            .await
    }

    /// Page raw query output after applying a caller-supplied text transform.
    /// The transform runs before filtering or returning values, allowing the
    /// runner to redact without persisting a second copy of query results.
    pub async fn page_query_rows_mapped<F>(
        &self,
        id: InvocationId,
        filter: Option<&str>,
        page: PageRequest,
        transform: F,
    ) -> Result<(Page<QueryRow>, u64, u64), StoreError>
    where
        F: Fn(&str) -> String + Send + 'static,
    {
        let known_total = {
            let index = self.inner.index.lock().await;
            let entry = index.entries.get(&id).ok_or(StoreError::NotFound(id))?;
            filter
                .is_none()
                .then(|| {
                    entry
                        .record
                        .summary
                        .as_ref()
                        .and_then(|summary| summary.query_result_count)
                })
                .flatten()
        };
        let invocation_id = id.to_string();
        let cursor = page
            .cursor
            .as_deref()
            .map(|value| FileCursor::decode_for(value, "query_rows", &invocation_id, filter))
            .transpose()?;
        let start_offset = cursor.as_ref().map_or(0, |value| value.offset);
        let start_ordinal = cursor
            .as_ref()
            .map_or(0, |value| value.ordinal.saturating_add(1));
        let path = self.paths_for_id(id).stdout;
        let filter = filter.map(str::to_owned);
        let limit = page.limit.clamp(1, 100) as usize;
        tokio::task::spawn_blocking(move || {
            page_query_file(
                &path,
                &invocation_id,
                filter.as_deref(),
                QueryFilePage {
                    start_offset,
                    start_ordinal,
                    limit,
                    known_total,
                },
                transform,
            )
        })
        .await?
    }

    async fn mutate<F>(&self, id: InvocationId, operation: F) -> Result<(), StoreError>
    where
        F: FnOnce(&mut DurableRecord) -> Result<(), StoreError>,
    {
        let mut index = self.inner.index.lock().await;
        ensure_exists(&index, id)?;
        let paths = self.paths_for_id(id);
        let mut durable = read_durable(&paths.metadata).await?;
        operation(&mut durable)?;
        persist_durable(&paths, &mut durable).await?;
        replace_index_entry(&mut index, id, durable.index_entry());
        Ok(())
    }

    async fn ensure_invocation(&self, id: InvocationId) -> Result<(), StoreError> {
        let index = self.inner.index.lock().await;
        ensure_exists(&index, id)
    }

    async fn read_full_invocation(&self, id: InvocationId) -> Result<InvocationRecord, StoreError> {
        self.ensure_invocation(id).await?;
        Ok(read_durable(&self.paths_for_id(id).metadata)
            .await?
            .invocation)
    }
}

fn ensure_exists(index: &Index, id: InvocationId) -> Result<(), StoreError> {
    index
        .entries
        .contains_key(&id)
        .then_some(())
        .ok_or(StoreError::NotFound(id))
}

fn insert_index_entry(index: &mut Index, id: InvocationId, entry: IndexEntry) {
    add_secondary_indexes(index, id, &entry);
    index.retained_bytes = index.retained_bytes.saturating_add(entry.evidence_bytes);
    index.entries.insert(id, entry);
}

fn replace_index_entry(index: &mut Index, id: InvocationId, entry: IndexEntry) {
    if let Some(previous) = index.entries.remove(&id) {
        remove_secondary_indexes(index, id, &previous);
        index.retained_bytes = index.retained_bytes.saturating_sub(previous.evidence_bytes);
    }
    add_secondary_indexes(index, id, &entry);
    index.entries.insert(id, entry.clone());
    index.retained_bytes = index.retained_bytes.saturating_add(entry.evidence_bytes);
}

fn remove_index_entry(index: &mut Index, id: InvocationId) {
    if let Some(entry) = index.entries.remove(&id) {
        remove_secondary_indexes(index, id, &entry);
        index.retained_bytes = index.retained_bytes.saturating_sub(entry.evidence_bytes);
    }
}

fn add_secondary_indexes(index: &mut Index, id: InvocationId, entry: &IndexEntry) {
    let requested = (entry.record.request.requested_at_ms, id);
    index.by_requested.insert(requested);
    index
        .by_workspace
        .entry(entry.record.request.workspace.clone())
        .or_default()
        .insert(requested);
    if let Some(deferred) = &entry.deferred {
        index
            .deferred_by_created
            .insert((deferred.created_at_ms, id));
    }
    if entry.record.state.is_terminal() {
        index
            .terminal_by_finished
            .insert((entry.record.finished_at_ms.unwrap_or(i64::MIN), id));
    }
}

fn remove_secondary_indexes(index: &mut Index, id: InvocationId, entry: &IndexEntry) {
    let requested = (entry.record.request.requested_at_ms, id);
    index.by_requested.remove(&requested);
    let workspace = entry.record.request.workspace.clone();
    let remove_workspace = index
        .by_workspace
        .get_mut(&workspace)
        .is_some_and(|entries| {
            entries.remove(&requested);
            entries.is_empty()
        });
    if remove_workspace {
        index.by_workspace.remove(&workspace);
    }
    if let Some(deferred) = &entry.deferred {
        index
            .deferred_by_created
            .remove(&(deferred.created_at_ms, id));
    }
    if entry.record.state.is_terminal() {
        index
            .terminal_by_finished
            .remove(&(entry.record.finished_at_ms.unwrap_or(i64::MIN), id));
    }
}

async fn persist_durable(
    paths: &InvocationPaths,
    durable: &mut DurableRecord,
) -> Result<(), StoreError> {
    let payload_bytes = evidence_payload_size(paths).await?;
    let mut accounted = payload_bytes;
    // The decimal byte count changes record length by at most a few digits;
    // converge before the one atomic write rather than committing twice.
    for _ in 0..4 {
        durable.evidence_bytes = accounted;
        let record_bytes = u64::try_from(serde_json::to_vec(durable)?.len()).unwrap_or(u64::MAX);
        let next = payload_bytes.saturating_add(record_bytes);
        if next == accounted {
            break;
        }
        accounted = next;
    }
    durable.evidence_bytes = accounted;
    write_json_atomic(&paths.metadata, durable).await
}

async fn evidence_payload_size(paths: &InvocationPaths) -> Result<u64, StoreError> {
    let mut size = 0_u64;
    for path in [
        &paths.request,
        &paths.stdout,
        &paths.stderr,
        &paths.bep,
        &paths.summary,
        &paths.artifacts,
    ] {
        match tokio::fs::symlink_metadata(path).await {
            Ok(metadata) if metadata.file_type().is_file() => {
                size = size.saturating_add(metadata.len());
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(size)
}

async fn evidence_size(paths: &InvocationPaths) -> Result<u64, StoreError> {
    let mut size = 0_u64;
    for path in [
        &paths.request,
        &paths.metadata,
        &paths.stdout,
        &paths.stderr,
        &paths.bep,
        &paths.summary,
        &paths.artifacts,
    ] {
        match tokio::fs::symlink_metadata(path).await {
            Ok(metadata) if metadata.file_type().is_file() => {
                size = size.saturating_add(metadata.len());
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(size)
}

async fn read_durable(path: &Path) -> Result<DurableRecord, StoreError> {
    let bytes = tokio::fs::read(path).await?;
    decode_durable(path, &bytes)
}

fn decode_durable(path: &Path, bytes: &[u8]) -> Result<DurableRecord, StoreError> {
    let durable: DurableRecord =
        serde_json::from_slice(bytes).map_err(|error| StoreError::CorruptRecord {
            path: path.to_owned(),
            message: error.to_string(),
        })?;
    if durable.schema_version != RECORD_SCHEMA_VERSION {
        return Err(StoreError::UnsupportedRecordSchema {
            found: durable.schema_version,
            path: path.to_owned(),
        });
    }
    Ok(durable)
}

async fn recover_trash(cache_root: &Path) -> Result<(), StoreError> {
    let trash = cache_root.join("trash");
    let mut entries = tokio::fs::read_dir(&trash).await?;
    while let Some(entry) = entries.next_entry().await? {
        let metadata = tokio::fs::symlink_metadata(entry.path()).await?;
        if metadata.is_dir() {
            tokio::fs::remove_dir_all(entry.path()).await?;
        } else {
            tokio::fs::remove_file(entry.path()).await?;
        }
    }
    Ok(())
}

async fn load_index(cache_root: &Path) -> Result<Index, StoreError> {
    let cache_root = cache_root.to_owned();
    tokio::task::spawn_blocking(move || load_index_blocking(&cache_root)).await?
}

fn load_index_blocking(cache_root: &Path) -> Result<Index, StoreError> {
    let mut index = Index::default();
    let invocations = cache_root.join("invocations");
    for day in std::fs::read_dir(&invocations)? {
        let day = day?;
        if !day.file_type()?.is_dir() {
            continue;
        }
        for shard in std::fs::read_dir(day.path())? {
            let shard = shard?;
            if !shard.file_type()?.is_dir() {
                continue;
            }
            for directory in std::fs::read_dir(shard.path())? {
                let directory = directory?;
                if !directory.file_type()?.is_dir() {
                    continue;
                }
                let name = directory.file_name().to_string_lossy().into_owned();
                let id = parse_id(&name).ok_or_else(|| StoreError::CorruptRecord {
                    path: directory.path(),
                    message: "directory name is not an invocation UUID".into(),
                })?;
                let expected = InvocationPaths::new(cache_root, id);
                if expected.directory != directory.path() {
                    return Err(StoreError::CorruptRecord {
                        path: directory.path(),
                        message: "invocation is outside its UUIDv7 bucket or shard".into(),
                    });
                }
                for temporary in [
                    expected.metadata.with_extension("tmp"),
                    expected.request.with_extension("tmp"),
                    expected.summary.with_extension("tmp"),
                    expected.artifacts.with_extension("tmp"),
                ] {
                    let _ = std::fs::remove_file(temporary);
                }
                match std::fs::read(&expected.metadata)
                    .map_err(StoreError::from)
                    .and_then(|bytes| decode_durable(&expected.metadata, &bytes))
                {
                    Ok(mut durable) => {
                        if durable.invocation.request.id != id {
                            return Err(StoreError::CorruptRecord {
                                path: expected.metadata,
                                message: "record ID does not match directory".into(),
                            });
                        }
                        // Terminal records commit byte accounting after every
                        // evidence-producing operation. Only a nonterminal
                        // record can have grown since its last commit.
                        if !durable.invocation.state.is_terminal() || durable.evidence_bytes == 0 {
                            durable.evidence_bytes = evidence_size_blocking(&expected)?;
                        }
                        insert_index_entry(&mut index, id, durable.index_entry());
                    }
                    Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                        // Creation is committed by record.json. A directory without
                        // it is an uncommitted crash remnant.
                        std::fs::remove_dir_all(directory.path())?;
                    }
                    Err(error) => return Err(error),
                }
            }
        }
    }
    Ok(index)
}

fn evidence_size_blocking(paths: &InvocationPaths) -> Result<u64, StoreError> {
    let mut size = 0_u64;
    for path in [
        &paths.request,
        &paths.metadata,
        &paths.stdout,
        &paths.stderr,
        &paths.bep,
        &paths.summary,
        &paths.artifacts,
    ] {
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_file() => {
                size = size.saturating_add(metadata.len());
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(size)
}

async fn recover_interrupted(cache_root: &Path, index: &mut Index) -> Result<(), StoreError> {
    let ids: Vec<_> = index
        .entries
        .iter()
        .filter_map(|(id, entry)| (!entry.record.state.is_terminal()).then_some(*id))
        .collect();
    for id in ids {
        let paths = InvocationPaths::new(cache_root, id);
        let mut durable = read_durable(&paths.metadata).await?;
        durable.invocation.state = InvocationState::Interrupted;
        durable.invocation.finished_at_ms = Some(bazel_mcp_types::unix_timestamp_ms());
        durable.invocation.termination = Some(Termination::Interrupted);
        durable.invocation.summary = Some(InvocationSummary {
            success: false,
            headline: "Invocation was interrupted when the previous server stopped".into(),
            truncated: true,
            inspect_hint: Some(format!(
                "Inspect invocation {id} for preserved raw evidence"
            )),
            ..InvocationSummary::default()
        });
        if let Some(deferred) = durable.deferred.as_mut() {
            deferred.extend_terminal_expiry(
                durable
                    .invocation
                    .finished_at_ms
                    .unwrap_or_else(bazel_mcp_types::unix_timestamp_ms),
            );
        }
        paths.write_summary(&durable.invocation).await?;
        persist_durable(&paths, &mut durable).await?;
        replace_index_entry(index, id, durable.index_entry());
    }
    Ok(())
}

async fn rename_to_trash(
    cache_root: &Path,
    id: InvocationId,
) -> Result<Option<PathBuf>, StoreError> {
    let source = InvocationPaths::new(cache_root, id).directory;
    let target = cache_root.join("trash").join(id.to_string());
    match tokio::fs::rename(&source, &target).await {
        Ok(()) => Ok(Some(target)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn evidence_trash(cache_root: &Path, id: InvocationId) -> PathBuf {
    cache_root.join("trash").join(format!("{id}.evidence"))
}

async fn stage_raw_evidence(
    cache_root: &Path,
    id: InvocationId,
    paths: &InvocationPaths,
) -> Result<(), StoreError> {
    let trash = evidence_trash(cache_root, id);
    match tokio::fs::remove_dir_all(&trash).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    tokio::fs::create_dir(&trash).await?;
    set_private_directory(&trash).await?;
    for source in [&paths.stdout, &paths.stderr, &paths.bep, &paths.artifacts] {
        let Some(name) = source.file_name() else {
            continue;
        };
        match tokio::fs::rename(source, trash.join(name)).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

async fn finish_staged_evidence(cache_root: &Path, id: InvocationId) -> Result<(), StoreError> {
    match tokio::fs::remove_dir_all(evidence_trash(cache_root, id)).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

async fn read_json_or_default<T>(path: &Path) -> Result<T, StoreError>
where
    T: DeserializeOwned + Default,
{
    match tokio::fs::read(path).await {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(error.into()),
    }
}

fn page_records<T, F>(
    scope: &str,
    id: InvocationId,
    filter: Option<&str>,
    page: PageRequest,
    records: Vec<T>,
    searchable: F,
) -> Result<(Page<T>, u64, u64), StoreError>
where
    F: Fn(&T) -> String,
{
    let limit = page.limit.clamp(1, 100) as usize;
    let invocation_id = id.to_string();
    let after = page
        .cursor
        .as_deref()
        .map(|value| OrdinalCursor::decode_for(value, scope, &invocation_id, filter))
        .transpose()?
        .map_or(-1, |cursor| cursor.ordinal);
    let total = records.len() as u64;
    let filtered = records
        .iter()
        .filter(|record| filter.is_none_or(|filter| searchable(record).contains(filter)))
        .count() as u64;
    let mut selected: Vec<_> = records
        .into_iter()
        .enumerate()
        .filter(|(ordinal, record)| {
            i64::try_from(*ordinal).unwrap_or(i64::MAX) > after
                && filter.is_none_or(|filter| searchable(record).contains(filter))
        })
        .take(limit + 1)
        .collect();
    let truncated = selected.len() > limit;
    selected.truncate(limit);
    let next_cursor = if truncated {
        selected
            .last()
            .map(|(ordinal, _)| {
                OrdinalCursor::new(
                    scope,
                    &invocation_id,
                    filter,
                    i64::try_from(*ordinal).unwrap_or(i64::MAX),
                )
                .encode()
            })
            .transpose()?
    } else {
        None
    };
    Ok((
        Page {
            items: selected.into_iter().map(|(_, record)| record).collect(),
            next_cursor,
            truncated,
        },
        total,
        filtered,
    ))
}

struct QueryFilePage {
    start_offset: u64,
    start_ordinal: u64,
    limit: usize,
    known_total: Option<u64>,
}

fn page_query_file<F>(
    path: &Path,
    invocation_id: &str,
    filter: Option<&str>,
    request: QueryFilePage,
    transform: F,
) -> Result<(Page<QueryRow>, u64, u64), StoreError>
where
    F: Fn(&str) -> String,
{
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((
                Page {
                    items: Vec::new(),
                    next_cursor: None,
                    truncated: false,
                },
                0,
                0,
            ));
        }
        Err(error) => return Err(error.into()),
    };
    if filter.is_none()
        && let Some(total) = request.known_total
    {
        return page_unfiltered_query_file(
            file,
            invocation_id,
            request.start_offset,
            request.start_ordinal,
            request.limit,
            total,
            transform,
        );
    }
    let mut reader = BoundedLineReader::new(BufReader::new(file), QUERY_LINE_LIMIT);
    let mut total = 0_u64;
    let mut filtered = 0_u64;
    let mut selected = Vec::with_capacity(request.limit + 1);
    while let Some(mut line) = reader.next_line()? {
        total = total.saturating_add(1);
        line.value = transform(&line.value);
        let matches = filter.is_none_or(|filter| line.value.contains(filter));
        if matches {
            filtered = filtered.saturating_add(1);
            if line.start_offset >= request.start_offset && selected.len() <= request.limit {
                selected.push(line);
            }
        }
    }
    let truncated = selected.len() > request.limit;
    selected.truncate(request.limit);
    let next_cursor = if truncated {
        selected
            .last()
            .map(|line| {
                FileCursor::new(
                    "query_rows",
                    invocation_id,
                    filter,
                    line.end_offset,
                    line.ordinal,
                )
                .encode()
            })
            .transpose()?
    } else {
        None
    };
    Ok((
        Page {
            items: selected
                .into_iter()
                .map(|line| QueryRow {
                    ordinal: line.ordinal,
                    value: line.value,
                })
                .collect(),
            next_cursor,
            truncated,
        },
        total,
        filtered,
    ))
}

fn page_unfiltered_query_file<F>(
    mut file: File,
    invocation_id: &str,
    start_offset: u64,
    start_ordinal: u64,
    limit: usize,
    total: u64,
    transform: F,
) -> Result<(Page<QueryRow>, u64, u64), StoreError>
where
    F: Fn(&str) -> String,
{
    use std::io::{Seek, SeekFrom};

    file.seek(SeekFrom::Start(start_offset))?;
    let mut reader = BoundedLineReader::with_position(
        BufReader::new(file),
        QUERY_LINE_LIMIT,
        start_offset,
        start_ordinal,
    );
    let mut selected = Vec::with_capacity(limit + 1);
    while selected.len() <= limit {
        let Some(mut line) = reader.next_line()? else {
            break;
        };
        line.value = transform(&line.value);
        selected.push(line);
    }
    let truncated = selected.len() > limit;
    selected.truncate(limit);
    let next_cursor = if truncated {
        selected
            .last()
            .map(|line| {
                FileCursor::new(
                    "query_rows",
                    invocation_id,
                    None,
                    line.end_offset,
                    line.ordinal,
                )
                .encode()
            })
            .transpose()?
    } else {
        None
    };
    Ok((
        Page {
            items: selected
                .into_iter()
                .map(|line| QueryRow {
                    ordinal: line.ordinal,
                    value: line.value,
                })
                .collect(),
            next_cursor,
            truncated,
        },
        total,
        total,
    ))
}

struct BoundedLineReader<R> {
    reader: R,
    offset: u64,
    ordinal: u64,
    limit: usize,
}

struct BoundedLine {
    start_offset: u64,
    end_offset: u64,
    ordinal: u64,
    value: String,
}

impl<R: BufRead> BoundedLineReader<R> {
    fn new(reader: R, limit: usize) -> Self {
        Self::with_position(reader, limit, 0, 0)
    }

    fn with_position(reader: R, limit: usize, offset: u64, ordinal: u64) -> Self {
        Self {
            reader,
            offset,
            ordinal,
            limit,
        }
    }

    fn next_line(&mut self) -> std::io::Result<Option<BoundedLine>> {
        let start_offset = self.offset;
        let ordinal = self.ordinal;
        let mut value = Vec::new();
        let mut saw_bytes = false;
        loop {
            let (consumed, newline, reached_eof) = {
                let available = self.reader.fill_buf()?;
                if available.is_empty() {
                    (0, false, true)
                } else if let Some(position) = available.iter().position(|byte| *byte == b'\n') {
                    let consumed = position + 1;
                    let copy = position.min(self.limit.saturating_sub(value.len()));
                    value.extend_from_slice(&available[..copy]);
                    (consumed, true, false)
                } else {
                    let consumed = available.len();
                    let copy = consumed.min(self.limit.saturating_sub(value.len()));
                    value.extend_from_slice(&available[..copy]);
                    (consumed, false, false)
                }
            };
            if reached_eof {
                if !saw_bytes {
                    return Ok(None);
                }
                break;
            }
            saw_bytes = true;
            self.reader.consume(consumed);
            self.offset = self
                .offset
                .saturating_add(u64::try_from(consumed).unwrap_or(u64::MAX));
            if newline {
                break;
            }
        }
        if value.last() == Some(&b'\r') {
            value.pop();
        }
        self.ordinal = self.ordinal.saturating_add(1);
        Ok(Some(BoundedLine {
            start_offset,
            end_offset: self.offset,
            ordinal,
            value: String::from_utf8_lossy(&value).into_owned(),
        }))
    }
}

fn compact_record(record: &InvocationRecord) -> InvocationRecord {
    let mut compact = record.clone();
    if let Some(summary) = compact.summary.as_mut() {
        summary.targets.clear();
        summary.tests.clear();
        if let Some(coverage) = summary.coverage.as_mut() {
            coverage.files.clear();
        }
    }
    compact
}

fn transition_lost_evidence(
    index: &mut Index,
    id: InvocationId,
    next: InvocationState,
    termination: Option<Termination>,
    summary: Option<InvocationSummary>,
) -> Result<InvocationRecord, StoreError> {
    let mut record = index
        .entries
        .get(&id)
        .ok_or(StoreError::NotFound(id))?
        .record
        .clone();
    record.transition(next)?;
    record.termination = termination;
    record.summary = summary;
    replace_index_entry(
        index,
        id,
        IndexEntry {
            record: record.clone(),
            deferred: None,
            evidence_bytes: 0,
        },
    );
    Ok(record)
}

fn error_is_not_found(error: &StoreError) -> bool {
    matches!(error, StoreError::Io(error) if error.kind() == std::io::ErrorKind::NotFound)
}

fn parse_id(value: &str) -> Option<InvocationId> {
    serde_json::from_str::<InvocationId>(&format!("\"{value}\"")).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bazel_mcp_types::{BazelCommand, InvocationRequest};
    use tempfile::TempDir;

    fn record(workspace: &Path) -> InvocationRecord {
        InvocationRecord::queued(InvocationRequest::new(
            workspace.to_owned(),
            BazelCommand::Build,
            vec!["//...".into()],
        ))
    }

    async fn succeed(store: &Store, id: InvocationId) {
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
                None,
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn lifecycle_survives_restart_and_uses_uuid_buckets() {
        let root = TempDir::new().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let record = record(root.path());
        let id = record.request.id;
        let paths = store.create_invocation(&record).await.unwrap();
        assert!(paths.directory.starts_with(root.path().join("invocations")));
        store
            .transition(id, InvocationState::Starting, None, None)
            .await
            .unwrap();
        drop(store);

        let reopened = Store::open(root.path()).await.unwrap();
        let recovered = reopened.get_invocation(id).await.unwrap();
        assert_eq!(recovered.state, InvocationState::Interrupted);
        assert_eq!(recovered.termination, Some(Termination::Interrupted));
    }

    #[tokio::test]
    async fn exclusive_lock_rejects_a_second_writer() {
        let root = TempDir::new().unwrap();
        let _first = Store::open(root.path()).await.unwrap();
        assert!(matches!(
            Store::open(root.path()).await,
            Err(StoreError::Locked(_))
        ));
    }

    #[tokio::test]
    async fn startup_rebuilds_workspace_and_deferred_ordering_indexes() {
        let root = TempDir::new().unwrap();
        let workspace_a = root.path().join("a");
        let workspace_b = root.path().join("b");
        let store = Store::open(root.path()).await.unwrap();
        let mut first = record(&workspace_a);
        first.request.requested_at_ms = 100;
        let first_id = first.request.id;
        store.create_invocation(&first).await.unwrap();
        let mut second = record(&workspace_b);
        second.request.requested_at_ms = 200;
        store.create_invocation(&second).await.unwrap();
        let mut third = record(&workspace_a);
        third.request.requested_at_ms = 300;
        let third_id = third.request.id;
        let deferred =
            DeferredResultRecord::new(third_id, DeferredRetrieval::InlineResult, 300, i64::MAX);
        store
            .create_invocation_with_deferred(&third, Some(&deferred))
            .await
            .unwrap();

        let workspace_page = store
            .list_invocations(
                Some(&workspace_a),
                PageRequest {
                    cursor: None,
                    limit: 10,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            workspace_page
                .items
                .iter()
                .map(|record| record.request.id)
                .collect::<Vec<_>>(),
            vec![third_id, first_id]
        );
        let deferred_page = store
            .list_deferred_results(DeferredRetrieval::InlineResult, 301, PageRequest::default())
            .await
            .unwrap();
        assert_eq!(deferred_page.items[0].deferred.invocation_id, third_id);
        drop(store);

        let reopened = Store::open(root.path()).await.unwrap();
        let rebuilt = reopened
            .list_invocations(Some(&workspace_a), PageRequest::default())
            .await
            .unwrap();
        assert_eq!(rebuilt.items[0].request.id, third_id);
        let rebuilt_deferred = reopened
            .list_deferred_results(DeferredRetrieval::InlineResult, 301, PageRequest::default())
            .await
            .unwrap();
        assert_eq!(rebuilt_deferred.items[0].deferred.invocation_id, third_id);
    }

    #[tokio::test]
    async fn query_pages_are_read_from_stdout_with_bounded_lines() {
        let root = TempDir::new().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let record = record(root.path());
        let id = record.request.id;
        let paths = store.create_invocation(&record).await.unwrap();
        tokio::fs::write(&paths.stdout, b"one\ntwo needle\nthree needle\n")
            .await
            .unwrap();
        let first = store
            .page_query_rows(
                id,
                Some("needle"),
                PageRequest {
                    cursor: None,
                    limit: 1,
                },
            )
            .await
            .unwrap();
        assert_eq!(first.0.items[0].ordinal, 1);
        assert_eq!((first.1, first.2), (3, 2));
        assert!(first.0.truncated);
        let second = store
            .page_query_rows(
                id,
                Some("needle"),
                PageRequest {
                    cursor: first.0.next_cursor,
                    limit: 1,
                },
            )
            .await
            .unwrap();
        assert_eq!(second.0.items[0].ordinal, 2);
    }

    #[tokio::test]
    async fn quota_gc_renames_then_removes_terminal_directories() {
        let root = TempDir::new().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let record = record(root.path());
        let id = record.request.id;
        let paths = store.create_invocation(&record).await.unwrap();
        tokio::fs::write(&paths.stdout, vec![b'x'; 4096])
            .await
            .unwrap();
        succeed(&store, id).await;
        assert_eq!(
            store
                .enforce_retention(Duration::from_secs(60), 1)
                .await
                .unwrap(),
            1
        );
        assert!(matches!(
            store.get_invocation(id).await,
            Err(StoreError::NotFound(_))
        ));
        assert!(!paths.directory.exists());
    }

    #[tokio::test]
    async fn quota_gc_accounts_for_but_never_evicts_live_evidence() {
        let root = TempDir::new().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let record = record(root.path());
        let id = record.request.id;
        let paths = store.create_invocation(&record).await.unwrap();
        store
            .transition(id, InvocationState::Starting, None, None)
            .await
            .unwrap();
        store
            .transition(id, InvocationState::Running, None, None)
            .await
            .unwrap();
        tokio::fs::write(&paths.stdout, vec![b'x'; 1024 * 1024])
            .await
            .unwrap();
        assert_eq!(
            store
                .enforce_retention(Duration::from_secs(0), 1)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            store.get_invocation(id).await.unwrap().state,
            InvocationState::Running
        );
        assert!(paths.stdout.exists());
    }

    #[tokio::test]
    async fn restart_discards_uncommitted_temporary_records() {
        let root = TempDir::new().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let record = record(root.path());
        let id = record.request.id;
        let paths = store.create_invocation(&record).await.unwrap();
        succeed(&store, id).await;
        drop(store);

        tokio::fs::write(paths.metadata.with_extension("tmp"), b"truncated")
            .await
            .unwrap();
        let reopened = Store::open(root.path()).await.unwrap();
        assert_eq!(
            reopened.get_invocation(id).await.unwrap().state,
            InvocationState::Succeeded
        );
        assert!(!paths.metadata.with_extension("tmp").exists());
    }

    #[tokio::test]
    async fn restart_finishes_two_phase_deletions_and_removes_uncommitted_directories() {
        let root = TempDir::new().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let committed = record(root.path());
        let committed_id = committed.request.id;
        let committed_paths = store.create_invocation(&committed).await.unwrap();
        succeed(&store, committed_id).await;
        drop(store);

        let trash = root.path().join("trash").join(committed_id.to_string());
        tokio::fs::rename(&committed_paths.directory, &trash)
            .await
            .unwrap();
        let orphan = root
            .path()
            .join("invocations/00000000/00/00000000-0000-7000-8000-000000000000");
        tokio::fs::create_dir_all(&orphan).await.unwrap();
        let reopened = Store::open(root.path()).await.unwrap();
        assert!(matches!(
            reopened.get_invocation(committed_id).await,
            Err(StoreError::NotFound(_))
        ));
        assert!(!trash.exists());
        assert!(!orphan.exists());
    }

    #[tokio::test]
    async fn deferred_results_are_protected_until_terminal_expiry() {
        let root = TempDir::new().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let record = record(root.path());
        let id = record.request.id;
        let now = bazel_mcp_types::unix_timestamp_ms();
        let deferred = DeferredResultRecord::new(
            id,
            DeferredRetrieval::SeparateResult,
            now,
            now.saturating_add(60_000),
        );
        let paths = store
            .create_invocation_with_deferred(&record, Some(&deferred))
            .await
            .unwrap();
        tokio::fs::write(&paths.stdout, vec![b'x'; 4096])
            .await
            .unwrap();
        succeed(&store, id).await;
        assert_eq!(
            store
                .enforce_retention(Duration::from_secs(0), 0)
                .await
                .unwrap(),
            1
        );
        assert!(store.get_invocation(id).await.is_ok());
        assert!(!paths.stdout.exists());
        store
            .extend_deferred_expiry(id, now, i64::MAX)
            .await
            .unwrap();
        // Force terminal expiry without waiting; deletion clears protection.
        store
            .mutate(id, |durable| {
                durable.deferred.as_mut().unwrap().expires_at_ms = i64::MIN;
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(store.delete_expired_deferred_results(now).await.unwrap(), 1);
        assert_eq!(
            store
                .enforce_retention(Duration::from_secs(0), 0)
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn corrupt_committed_records_fail_closed_without_overwriting_evidence() {
        let root = TempDir::new().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let record = record(root.path());
        let paths = store.create_invocation(&record).await.unwrap();
        drop(store);
        let corrupt = b"{not-json";
        tokio::fs::write(&paths.metadata, corrupt).await.unwrap();
        assert!(matches!(
            Store::open(root.path()).await,
            Err(StoreError::CorruptRecord { .. })
        ));
        assert_eq!(tokio::fs::read(paths.metadata).await.unwrap(), corrupt);
    }

    #[tokio::test]
    async fn query_reader_caps_a_single_adversarial_line() {
        let root = TempDir::new().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let record = record(root.path());
        let id = record.request.id;
        let paths = store.create_invocation(&record).await.unwrap();
        tokio::fs::write(&paths.stdout, vec![b'x'; 2 * QUERY_LINE_LIMIT])
            .await
            .unwrap();
        let (page, total, filtered) = store
            .page_query_rows(id, None, PageRequest::default())
            .await
            .unwrap();
        assert_eq!((total, filtered), (1, 1));
        assert_eq!(page.items[0].value.len(), QUERY_LINE_LIMIT);
    }
}
