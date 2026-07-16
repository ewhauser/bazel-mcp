use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions},
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    sync::{
        Arc, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

#[cfg(test)]
use std::sync::atomic::AtomicBool;

use bazel_mcp_types::{
    Artifact, CoverageFile, DeferredFailure, DeferredResultRecord, DeferredResultView,
    DeferredRetrieval, DeferredTerminalState, Diagnostic, InvocationId, InvocationMetrics,
    InvocationRecord, InvocationState, InvocationSummary, Page, PageRequest, QueryRow,
    TargetResult, Termination, TestResult,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;
use tokio::sync::{Mutex, RwLock};

use crate::{
    InvocationPaths,
    cursor::{DeferredCursor, FileCursor, InvocationCursor, OrdinalCursor},
    files::{
        remove_if_exists, set_private_directory, set_private_file, write_bytes_atomic,
        write_json_atomic,
    },
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
    index: RwLock<Index>,
    mutation_locks: Mutex<BTreeMap<InvocationId, Weak<Mutex<()>>>>,
    manifest_commits: AtomicU64,
    manifest_bytes_written: AtomicU64,
    payload_recounts: AtomicU64,
    gc_renames: AtomicU64,
    gc_unlinks: AtomicU64,
    gc_rename_us: AtomicU64,
    gc_index_write_us: AtomicU64,
    gc_unlink_us: AtomicU64,
    #[cfg(test)]
    fail_next_gc_unlink: AtomicBool,
    startup_stats: StoreStartupStats,
    _lock_file: File,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StoreIoStats {
    pub manifest_commits: u64,
    pub manifest_bytes_written: u64,
    pub payload_recounts: u64,
    pub gc_renames: u64,
    pub gc_unlinks: u64,
    pub gc_rename_us: u64,
    pub gc_index_write_us: u64,
    pub gc_unlink_us: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StoreStartupStats {
    pub directory_traversal_us: u64,
    pub manifest_read_us: u64,
    pub manifest_decode_us: u64,
    pub index_build_us: u64,
}

/// One coalesced terminal metadata commit for a completed Bazel invocation.
pub struct InvocationCompletion {
    pub state: InvocationState,
    pub termination: Termination,
    pub summary: InvocationSummary,
    pub metrics: InvocationMetrics,
    pub canonical_arguments: Option<Vec<String>>,
    pub artifacts: Vec<Artifact>,
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
    retained_bytes: u64,
    telemetry_generation: u64,
    telemetry_flush_scheduled: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DurableRecord {
    schema_version: u32,
    invocation: InvocationRecord,
    #[serde(default)]
    deferred: Option<DeferredResultRecord>,
    #[serde(default)]
    payload_bytes: u64,
}

impl DurableRecord {
    fn index_entry(&self, retained_bytes: u64) -> IndexEntry {
        IndexEntry {
            record: compact_record(&self.invocation),
            deferred: self.deferred.clone(),
            retained_bytes,
            telemetry_generation: 0,
            telemetry_flush_scheduled: false,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct InvocationDetails {
    targets: Vec<TargetResult>,
    tests: Vec<TestResult>,
    coverage_files: Vec<CoverageFile>,
}

impl InvocationDetails {
    fn from_record(record: &InvocationRecord) -> Self {
        let Some(summary) = &record.summary else {
            return Self::default();
        };
        Self {
            targets: summary.targets.clone(),
            tests: summary.tests.clone(),
            coverage_files: summary
                .coverage
                .as_ref()
                .map_or_else(Vec::new, |coverage| coverage.files.clone()),
        }
    }

    fn hydrate(self, record: &mut InvocationRecord) {
        let Some(summary) = record.summary.as_mut() else {
            return;
        };
        summary.targets = self.targets;
        summary.tests = self.tests;
        if let Some(coverage) = summary.coverage.as_mut() {
            coverage.files = self.coverage_files;
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
        let (mut index, startup_stats) = load_index(&cache_root).await?;
        recover_interrupted(&cache_root, &mut index).await?;

        Ok(Self {
            cache_root,
            inner: Arc::new(StoreInner {
                index: RwLock::new(index),
                mutation_locks: Mutex::new(BTreeMap::new()),
                manifest_commits: AtomicU64::new(0),
                manifest_bytes_written: AtomicU64::new(0),
                payload_recounts: AtomicU64::new(0),
                gc_renames: AtomicU64::new(0),
                gc_unlinks: AtomicU64::new(0),
                gc_rename_us: AtomicU64::new(0),
                gc_index_write_us: AtomicU64::new(0),
                gc_unlink_us: AtomicU64::new(0),
                #[cfg(test)]
                fail_next_gc_unlink: AtomicBool::new(false),
                startup_stats,
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

    async fn mutation_lock(&self, id: InvocationId) -> Arc<Mutex<()>> {
        let mut locks = self.inner.mutation_locks.lock().await;
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(&id).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(Mutex::new(()));
        locks.insert(id, Arc::downgrade(&lock));
        lock
    }

    #[must_use]
    pub fn io_stats(&self) -> StoreIoStats {
        StoreIoStats {
            manifest_commits: self.inner.manifest_commits.load(Ordering::Relaxed),
            manifest_bytes_written: self.inner.manifest_bytes_written.load(Ordering::Relaxed),
            payload_recounts: self.inner.payload_recounts.load(Ordering::Relaxed),
            gc_renames: self.inner.gc_renames.load(Ordering::Relaxed),
            gc_unlinks: self.inner.gc_unlinks.load(Ordering::Relaxed),
            gc_rename_us: self.inner.gc_rename_us.load(Ordering::Relaxed),
            gc_index_write_us: self.inner.gc_index_write_us.load(Ordering::Relaxed),
            gc_unlink_us: self.inner.gc_unlink_us.load(Ordering::Relaxed),
        }
    }

    #[must_use]
    pub fn startup_stats(&self) -> StoreStartupStats {
        self.inner.startup_stats
    }

    async fn persist_durable(
        &self,
        paths: &InvocationPaths,
        durable: &mut DurableRecord,
        recount_payload: bool,
    ) -> Result<u64, StoreError> {
        self.inner.manifest_commits.fetch_add(1, Ordering::Relaxed);
        if recount_payload {
            self.inner.payload_recounts.fetch_add(1, Ordering::Relaxed);
        }
        let outcome = persist_durable(paths, durable, recount_payload).await?;
        self.inner
            .manifest_bytes_written
            .fetch_add(outcome.manifest_bytes, Ordering::Relaxed);
        Ok(outcome.retained_bytes)
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
        let id = record.request.id;
        let mutation_lock = self.mutation_lock(id).await;
        let _guard = mutation_lock.lock().await;
        if self.inner.index.read().await.entries.contains_key(&id) {
            return Err(StoreError::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "invocation already exists",
            )));
        }
        paths.create().await?;
        let result = async {
            let mut durable = DurableRecord {
                schema_version: RECORD_SCHEMA_VERSION,
                invocation: compact_record(record),
                deferred: deferred.cloned(),
                payload_bytes: 0,
            };
            let retained_bytes = self.persist_durable(&paths, &mut durable, true).await?;
            Ok::<_, StoreError>((durable, retained_bytes))
        }
        .await;
        let (durable, retained_bytes) = match result {
            Ok(durable) => durable,
            Err(error) => {
                let _ = tokio::fs::remove_dir_all(&paths.directory).await;
                return Err(error);
            }
        };
        let mut index = self.inner.index.write().await;
        insert_index_entry(&mut index, id, durable.index_entry(retained_bytes));
        Ok(paths)
    }

    pub async fn get_deferred_result(
        &self,
        id: InvocationId,
        retrieval: DeferredRetrieval,
        now_ms: i64,
    ) -> Result<DeferredResultView, StoreError> {
        let (deferred, record) = {
            let index = self.inner.index.read().await;
            let entry = index.entries.get(&id).ok_or(StoreError::NotFound(id))?;
            let Some(deferred) = entry.deferred.clone() else {
                return Err(StoreError::DeferredNotFound(id));
            };
            (deferred, entry.record.clone())
        };
        if deferred.retrieval != retrieval
            || deferred.is_expired(now_ms, record.state.is_terminal())
        {
            if deferred.is_expired(now_ms, record.state.is_terminal()) {
                self.mutate(id, false, |durable| {
                    durable.deferred = None;
                    Ok(())
                })
                .await?;
            }
            return Err(StoreError::DeferredNotFound(id));
        }
        let invocation = if record.state.is_terminal() {
            self.read_full_invocation(id).await?
        } else {
            record
        };
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
        let index = self.inner.index.read().await;
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
        self.mutate(id, false, |durable| {
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
        self.mutate(id, false, |durable| {
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
        self.mutate(id, false, |durable| {
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
        self.mutate(id, false, |durable| {
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
        let ids: Vec<_> = {
            let index = self.inner.index.read().await;
            index
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
                .collect()
        };
        for id in &ids {
            self.mutate(*id, false, |durable| {
                durable.deferred = None;
                Ok(())
            })
            .await?;
        }
        Ok(ids.len())
    }

    pub async fn get_invocation(&self, id: InvocationId) -> Result<InvocationRecord, StoreError> {
        self.inner
            .index
            .read()
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
        let mutation_lock = self.mutation_lock(id).await;
        let _guard = mutation_lock.lock().await;
        self.ensure_invocation(id).await?;
        let paths = self.paths_for_id(id);
        let (mut durable, _) = match read_durable(&paths.manifest).await {
            Ok(durable) => durable,
            Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut index = self.inner.index.write().await;
                return transition_lost_evidence(&mut index, id, next, termination, summary);
            }
            Err(error) => return Err(error),
        };
        let telemetry_generation = {
            let index = self.inner.index.read().await;
            merge_index_telemetry(&index, id, &mut durable.invocation.metrics)
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
        let result = durable.invocation.clone();
        if next.is_terminal() {
            write_details(&paths, &result).await?;
            durable.invocation = compact_record(&result);
        }
        let retained_bytes = match self
            .persist_durable(&paths, &mut durable, next.is_terminal())
            .await
        {
            Ok(retained_bytes) => retained_bytes,
            Err(error) if error_is_not_found(&error) => {
                let mut index = self.inner.index.write().await;
                return transition_lost_evidence(&mut index, id, next, termination, summary);
            }
            Err(error) => return Err(error),
        };
        let mut index = self.inner.index.write().await;
        replace_index_entry(&mut index, id, durable.index_entry(retained_bytes));
        mark_telemetry_flushed(&mut index, id, telemetry_generation);
        Ok(result)
    }

    pub async fn finish_invocation(
        &self,
        id: InvocationId,
        completion: InvocationCompletion,
    ) -> Result<InvocationRecord, StoreError> {
        let InvocationCompletion {
            state,
            termination,
            summary,
            metrics,
            canonical_arguments,
            artifacts,
        } = completion;
        if !state.is_terminal() {
            return Err(StoreError::State(bazel_mcp_types::StateTransitionError {
                current: self.get_invocation(id).await?.state,
                next: state,
            }));
        }
        let mutation_lock = self.mutation_lock(id).await;
        let _guard = mutation_lock.lock().await;
        self.ensure_invocation(id).await?;
        let paths = self.paths_for_id(id);
        let (mut durable, _) = read_durable(&paths.manifest).await?;
        durable.invocation.transition(state)?;
        durable.invocation.termination = Some(termination);
        durable.invocation.summary = Some(summary);
        durable.invocation.metrics = metrics;
        let telemetry_generation = {
            let index = self.inner.index.read().await;
            merge_index_telemetry(&index, id, &mut durable.invocation.metrics)
        };
        durable.invocation.canonical_arguments = canonical_arguments;
        if let Some(deferred) = durable.deferred.as_mut() {
            deferred.extend_terminal_expiry(
                durable
                    .invocation
                    .finished_at_ms
                    .unwrap_or_else(bazel_mcp_types::unix_timestamp_ms),
            );
        }
        let result = durable.invocation.clone();
        if artifacts.is_empty() {
            remove_if_exists(&paths.artifacts).await?;
        } else {
            write_json_atomic(&paths.artifacts, &artifacts).await?;
        }
        write_details(&paths, &result).await?;
        durable.invocation = compact_record(&result);
        let retained_bytes = self.persist_durable(&paths, &mut durable, true).await?;
        let mut index = self.inner.index.write().await;
        replace_index_entry(&mut index, id, durable.index_entry(retained_bytes));
        mark_telemetry_flushed(&mut index, id, telemetry_generation);
        Ok(result)
    }

    pub async fn record_model_visible_result(
        &self,
        id: InvocationId,
        bytes: u64,
        inspection: bool,
    ) -> Result<(), StoreError> {
        let schedule = {
            let mut index = self.inner.index.write().await;
            let entry = index.entries.get_mut(&id).ok_or(StoreError::NotFound(id))?;
            let metrics = &mut entry.record.metrics;
            metrics.model_visible_bytes = metrics.model_visible_bytes.saturating_add(bytes);
            if inspection {
                metrics.inspect_calls = metrics.inspect_calls.saturating_add(1);
            }
            entry.telemetry_generation = entry.telemetry_generation.saturating_add(1);
            if entry.telemetry_flush_scheduled {
                false
            } else {
                entry.telemetry_flush_scheduled = true;
                true
            }
        };
        if schedule {
            self.schedule_telemetry_flush(id);
        }
        Ok(())
    }

    pub async fn record_progress_notifications(
        &self,
        id: InvocationId,
        count: u64,
    ) -> Result<(), StoreError> {
        let schedule = {
            let mut index = self.inner.index.write().await;
            let entry = index.entries.get_mut(&id).ok_or(StoreError::NotFound(id))?;
            let metrics = &mut entry.record.metrics;
            metrics.progress_notifications = metrics.progress_notifications.saturating_add(count);
            entry.telemetry_generation = entry.telemetry_generation.saturating_add(1);
            if entry.telemetry_flush_scheduled {
                false
            } else {
                entry.telemetry_flush_scheduled = true;
                true
            }
        };
        if schedule {
            self.schedule_telemetry_flush(id);
        }
        Ok(())
    }

    fn schedule_telemetry_flush(&self, id: InvocationId) {
        let inner = Arc::downgrade(&self.inner);
        let cache_root = self.cache_root.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(250)).await;
                let Some(inner) = inner.upgrade() else {
                    return;
                };
                let store = Store {
                    cache_root: cache_root.clone(),
                    inner,
                };
                match store.flush_telemetry_once(id).await {
                    Ok(true) => return,
                    Ok(false) => {}
                    Err(_) => {
                        if let Some(entry) = store.inner.index.write().await.entries.get_mut(&id) {
                            entry.telemetry_flush_scheduled = false;
                        }
                        return;
                    }
                }
            }
        });
    }

    async fn flush_telemetry_once(&self, id: InvocationId) -> Result<bool, StoreError> {
        let mutation_lock = self.mutation_lock(id).await;
        let _guard = mutation_lock.lock().await;
        let (metrics, generation) = {
            let index = self.inner.index.read().await;
            let Some(entry) = index.entries.get(&id) else {
                return Ok(true);
            };
            if !entry.telemetry_flush_scheduled {
                return Ok(true);
            }
            (entry.record.metrics.clone(), entry.telemetry_generation)
        };
        let paths = self.paths_for_id(id);
        let (mut durable, _) = read_durable(&paths.manifest).await?;
        durable.invocation.metrics.model_visible_bytes = metrics.model_visible_bytes;
        durable.invocation.metrics.progress_notifications = metrics.progress_notifications;
        durable.invocation.metrics.inspect_calls = metrics.inspect_calls;
        let retained_bytes = self.persist_durable(&paths, &mut durable, false).await?;
        let mut index = self.inner.index.write().await;
        let (previous, clean) = {
            let Some(entry) = index.entries.get_mut(&id) else {
                return Ok(true);
            };
            let previous = entry.retained_bytes;
            entry.retained_bytes = retained_bytes;
            let clean = entry.telemetry_generation == generation;
            if clean {
                entry.telemetry_flush_scheduled = false;
            }
            (previous, clean)
        };
        index.retained_bytes = index.retained_bytes.saturating_sub(previous);
        index.retained_bytes = index.retained_bytes.saturating_add(retained_bytes);
        Ok(clean)
    }

    pub async fn update_cancellation_reason(
        &self,
        id: InvocationId,
        reason: &str,
    ) -> Result<(), StoreError> {
        self.mutate(id, false, |durable| {
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
        let index = self.inner.index.read().await;
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
        // Running evidence can grow without metadata commits. Refresh only the
        // bounded live set; terminal bytes remain commit-accounted, so normal
        // GC never walks the cache tree.
        let live_ids: Vec<_> = self
            .inner
            .index
            .read()
            .await
            .entries
            .iter()
            .filter_map(|(id, entry)| (!entry.record.state.is_terminal()).then_some(*id))
            .collect();
        for id in live_ids {
            let current = evidence_size(&self.paths_for_id(id)).await?;
            let mut index = self.inner.index.write().await;
            let previous = index.entries.get_mut(&id).and_then(|entry| {
                if entry.record.state.is_terminal() {
                    return None;
                }
                let previous = entry.retained_bytes;
                entry.retained_bytes = current;
                Some(previous)
            });
            if let Some(previous) = previous {
                index.retained_bytes = index.retained_bytes.saturating_sub(previous);
                index.retained_bytes = index.retained_bytes.saturating_add(current);
            }
        }
        let candidates: Vec<_> = {
            let index = self.inner.index.read().await;
            index
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
                .collect()
        };

        let low_watermark = maximum_bytes
            .saturating_mul(GC_LOW_WATERMARK_PERCENT)
            .checked_div(100)
            .unwrap_or(0);
        let mut reclaimed = 0;
        let mut processed = BTreeSet::new();
        for (id, finished, protected) in &candidates {
            if *finished < cutoff {
                if self.reclaim_terminal(*id, *protected).await? {
                    reclaimed += 1;
                }
                processed.insert(*id);
            }
        }
        if self.inner.index.read().await.retained_bytes > maximum_bytes {
            for (id, _, protected) in candidates {
                if self.inner.index.read().await.retained_bytes <= low_watermark {
                    break;
                }
                if processed.insert(id) && self.reclaim_terminal(id, protected).await? {
                    reclaimed += 1;
                }
            }
        }
        Ok(reclaimed)
    }

    async fn reclaim_terminal(
        &self,
        id: InvocationId,
        deferred_protected: bool,
    ) -> Result<bool, StoreError> {
        let mutation_lock = self.mutation_lock(id).await;
        let _guard = mutation_lock.lock().await;
        let (before, currently_protected) = {
            let index = self.inner.index.read().await;
            let Some(entry) = index.entries.get(&id) else {
                return Ok(false);
            };
            if !entry.record.state.is_terminal() {
                return Ok(false);
            }
            (entry.retained_bytes, entry.deferred.is_some())
        };
        if deferred_protected && currently_protected {
            let paths = self.paths_for_id(id);
            let (mut durable, _) = read_durable(&paths.manifest).await?;
            stage_raw_evidence(&self.cache_root, id, &paths).await?;
            let retained_bytes = self.persist_durable(&paths, &mut durable, true).await?;
            {
                let mut index = self.inner.index.write().await;
                replace_index_entry(&mut index, id, durable.index_entry(retained_bytes));
            }
            // The rename and manifest update committed pruning. Unlinking the
            // staged evidence is deliberately outside the shared index lock.
            let _ = finish_staged_evidence(&self.cache_root, id).await;
            return Ok(retained_bytes < before);
        }
        let rename_started = Instant::now();
        if let Some(trash) = rename_to_trash(&self.cache_root, id).await? {
            self.inner
                .gc_rename_us
                .fetch_add(elapsed_us(rename_started.elapsed()), Ordering::Relaxed);
            self.inner.gc_renames.fetch_add(1, Ordering::Relaxed);
            {
                let index_started = Instant::now();
                let mut index = self.inner.index.write().await;
                remove_index_entry(&mut index, id);
                self.inner
                    .gc_index_write_us
                    .fetch_add(elapsed_us(index_started.elapsed()), Ordering::Relaxed);
            }
            // Rename is the deletion commit. If unlinking fails, the index
            // stays removed and startup finishes this trash entry.
            let unlink_started = Instant::now();
            #[cfg(test)]
            let unlink_result = if self
                .inner
                .fail_next_gc_unlink
                .swap(false, Ordering::Relaxed)
            {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "injected GC unlink failure",
                ))
            } else {
                tokio::fs::remove_dir_all(trash).await
            };
            #[cfg(not(test))]
            let unlink_result = tokio::fs::remove_dir_all(trash).await;
            if unlink_result.is_ok() {
                self.inner.gc_unlinks.fetch_add(1, Ordering::Relaxed);
            }
            self.inner
                .gc_unlink_us
                .fetch_add(elapsed_us(unlink_started.elapsed()), Ordering::Relaxed);
            return Ok(true);
        }
        Ok(false)
    }

    /// Replace the artifact sidecar outside the normal coalesced terminal path.
    /// This is retained for tests and recovery-oriented callers; production
    /// completion writes artifacts through `finish_invocation`.
    pub async fn replace_artifacts(
        &self,
        id: InvocationId,
        artifacts: &[Artifact],
    ) -> Result<(), StoreError> {
        let mutation_lock = self.mutation_lock(id).await;
        let _guard = mutation_lock.lock().await;
        self.ensure_invocation(id).await?;
        let paths = self.paths_for_id(id);
        if artifacts.is_empty() {
            remove_if_exists(&paths.artifacts).await?;
        } else {
            write_json_atomic(&paths.artifacts, artifacts).await?;
        }
        let (mut durable, _) = read_durable(&paths.manifest).await?;
        let retained_bytes = self.persist_durable(&paths, &mut durable, true).await?;
        let mut index = self.inner.index.write().await;
        replace_index_entry(&mut index, id, durable.index_entry(retained_bytes));
        Ok(())
    }

    pub async fn page_diagnostics(
        &self,
        id: InvocationId,
        filter: Option<&str>,
        page: PageRequest,
    ) -> Result<(Page<Diagnostic>, u64, u64), StoreError> {
        let record = self.get_invocation(id).await?;
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
        let details = self.read_details(id).await?;
        let items = details.tests;
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
        let mutation_lock = self.mutation_lock(id).await;
        let _guard = mutation_lock.lock().await;
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
        let details = self.read_details(id).await?;
        let items = details.coverage_files;
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
        let mutation_lock = self.mutation_lock(id).await;
        let _guard = mutation_lock.lock().await;
        let known_total = {
            let index = self.inner.index.read().await;
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
        let maximum_bytes = page.max_bytes.unwrap_or(usize::MAX);
        tokio::task::spawn_blocking(move || {
            page_query_file(
                &path,
                &invocation_id,
                filter.as_deref(),
                QueryFilePage {
                    start_offset,
                    start_ordinal,
                    limit,
                    maximum_bytes,
                    known_total,
                },
                transform,
            )
        })
        .await?
    }

    async fn mutate<F>(
        &self,
        id: InvocationId,
        recount_payload: bool,
        operation: F,
    ) -> Result<(), StoreError>
    where
        F: FnOnce(&mut DurableRecord) -> Result<(), StoreError>,
    {
        let mutation_lock = self.mutation_lock(id).await;
        let _guard = mutation_lock.lock().await;
        self.ensure_invocation(id).await?;
        let paths = self.paths_for_id(id);
        let (mut durable, _) = read_durable(&paths.manifest).await?;
        let telemetry_generation = {
            let index = self.inner.index.read().await;
            merge_index_telemetry(&index, id, &mut durable.invocation.metrics)
        };
        operation(&mut durable)?;
        let retained_bytes = self
            .persist_durable(&paths, &mut durable, recount_payload)
            .await?;
        let mut index = self.inner.index.write().await;
        replace_index_entry(&mut index, id, durable.index_entry(retained_bytes));
        mark_telemetry_flushed(&mut index, id, telemetry_generation);
        Ok(())
    }

    async fn ensure_invocation(&self, id: InvocationId) -> Result<(), StoreError> {
        let index = self.inner.index.read().await;
        ensure_exists(&index, id)
    }

    async fn read_full_invocation(&self, id: InvocationId) -> Result<InvocationRecord, StoreError> {
        let mutation_lock = self.mutation_lock(id).await;
        let _guard = mutation_lock.lock().await;
        self.ensure_invocation(id).await?;
        let paths = self.paths_for_id(id);
        let (durable, _) = read_durable(&paths.manifest).await?;
        let mut record = durable.invocation;
        read_json_or_default::<InvocationDetails>(&paths.details)
            .await?
            .hydrate(&mut record);
        Ok(record)
    }

    async fn read_details(&self, id: InvocationId) -> Result<InvocationDetails, StoreError> {
        let mutation_lock = self.mutation_lock(id).await;
        let _guard = mutation_lock.lock().await;
        self.ensure_invocation(id).await?;
        read_json_or_default(&self.paths_for_id(id).details).await
    }
}

fn elapsed_us(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
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
    index.retained_bytes = index.retained_bytes.saturating_add(entry.retained_bytes);
    index.entries.insert(id, entry);
}

fn replace_index_entry(index: &mut Index, id: InvocationId, mut entry: IndexEntry) {
    if let Some(previous) = index.entries.remove(&id) {
        merge_telemetry(&previous.record.metrics, &mut entry.record.metrics);
        entry.telemetry_generation = previous.telemetry_generation;
        entry.telemetry_flush_scheduled = previous.telemetry_flush_scheduled;
        remove_secondary_indexes(index, id, &previous);
        index.retained_bytes = index.retained_bytes.saturating_sub(previous.retained_bytes);
    }
    add_secondary_indexes(index, id, &entry);
    index.retained_bytes = index.retained_bytes.saturating_add(entry.retained_bytes);
    index.entries.insert(id, entry);
}

fn remove_index_entry(index: &mut Index, id: InvocationId) {
    if let Some(entry) = index.entries.remove(&id) {
        remove_secondary_indexes(index, id, &entry);
        index.retained_bytes = index.retained_bytes.saturating_sub(entry.retained_bytes);
    }
}

fn merge_index_telemetry(index: &Index, id: InvocationId, metrics: &mut InvocationMetrics) -> u64 {
    if let Some(entry) = index.entries.get(&id) {
        merge_telemetry(&entry.record.metrics, metrics);
        entry.telemetry_generation
    } else {
        0
    }
}

fn mark_telemetry_flushed(index: &mut Index, id: InvocationId, generation: u64) {
    if let Some(entry) = index.entries.get_mut(&id)
        && entry.telemetry_generation == generation
    {
        entry.telemetry_flush_scheduled = false;
    }
}

fn merge_telemetry(source: &InvocationMetrics, destination: &mut InvocationMetrics) {
    destination.model_visible_bytes = destination
        .model_visible_bytes
        .max(source.model_visible_bytes);
    destination.progress_notifications = destination
        .progress_notifications
        .max(source.progress_notifications);
    destination.inspect_calls = destination.inspect_calls.max(source.inspect_calls);
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
    recount_payload: bool,
) -> Result<PersistOutcome, StoreError> {
    if recount_payload {
        durable.payload_bytes = evidence_payload_size(paths).await?;
    }
    let bytes = serde_json::to_vec(durable)?;
    let manifest_bytes = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    write_bytes_atomic(&paths.manifest, &bytes).await?;
    Ok(PersistOutcome {
        retained_bytes: durable.payload_bytes.saturating_add(manifest_bytes),
        manifest_bytes,
    })
}

struct PersistOutcome {
    retained_bytes: u64,
    manifest_bytes: u64,
}

async fn evidence_payload_size(paths: &InvocationPaths) -> Result<u64, StoreError> {
    let mut size = 0_u64;
    for path in [
        &paths.details,
        &paths.stdout,
        &paths.stderr,
        &paths.evidence,
        &paths.bep,
        &paths.artifacts,
        &paths.test_logs_raw,
        &paths.test_log_evidence,
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
        &paths.manifest,
        &paths.details,
        &paths.stdout,
        &paths.stderr,
        &paths.evidence,
        &paths.bep,
        &paths.artifacts,
        &paths.test_logs_raw,
        &paths.test_log_evidence,
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

async fn read_durable(path: &Path) -> Result<(DurableRecord, u64), StoreError> {
    let bytes = tokio::fs::read(path).await?;
    let manifest_bytes = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    Ok((decode_durable(path, &bytes)?, manifest_bytes))
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

async fn load_index(cache_root: &Path) -> Result<(Index, StoreStartupStats), StoreError> {
    let cache_root = cache_root.to_owned();
    tokio::task::spawn_blocking(move || load_index_blocking(&cache_root)).await?
}

fn load_index_blocking(cache_root: &Path) -> Result<(Index, StoreStartupStats), StoreError> {
    let total_started = Instant::now();
    let mut index = Index::default();
    let mut stats = StoreStartupStats::default();
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
                    expected.manifest.with_extension("tmp"),
                    expected.details.with_extension("tmp"),
                    expected.artifacts.with_extension("tmp"),
                    expected.evidence.with_extension("tmp"),
                    expected.test_logs_raw.with_extension("tmp"),
                    expected.test_log_evidence.with_extension("tmp"),
                ] {
                    let _ = std::fs::remove_file(temporary);
                }
                let read_started = Instant::now();
                let bytes = std::fs::read(&expected.manifest);
                stats.manifest_read_us = stats
                    .manifest_read_us
                    .saturating_add(elapsed_micros(read_started.elapsed()));
                match bytes {
                    Ok(bytes) => {
                        let manifest_bytes = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                        let decode_started = Instant::now();
                        let mut durable = decode_durable(&expected.manifest, &bytes)?;
                        stats.manifest_decode_us = stats
                            .manifest_decode_us
                            .saturating_add(elapsed_micros(decode_started.elapsed()));
                        if durable.invocation.request.id != id {
                            return Err(StoreError::CorruptRecord {
                                path: expected.manifest,
                                message: "record ID does not match directory".into(),
                            });
                        }
                        // Terminal records commit byte accounting after every
                        // evidence-producing operation. Only a nonterminal
                        // record can have grown since its last commit.
                        if !durable.invocation.state.is_terminal() {
                            durable.payload_bytes = evidence_payload_size_blocking(&expected)?;
                        }
                        let retained_bytes = durable.payload_bytes.saturating_add(manifest_bytes);
                        let index_started = Instant::now();
                        insert_index_entry(&mut index, id, durable.index_entry(retained_bytes));
                        stats.index_build_us = stats
                            .index_build_us
                            .saturating_add(elapsed_micros(index_started.elapsed()));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        // Creation is committed by manifest.json. A directory without
                        // it is an uncommitted crash remnant.
                        std::fs::remove_dir_all(directory.path())?;
                    }
                    Err(error) => return Err(error.into()),
                }
            }
        }
    }
    let total_us = elapsed_micros(total_started.elapsed());
    stats.directory_traversal_us = total_us.saturating_sub(
        stats
            .manifest_read_us
            .saturating_add(stats.manifest_decode_us)
            .saturating_add(stats.index_build_us),
    );
    Ok((index, stats))
}

fn elapsed_micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn evidence_payload_size_blocking(paths: &InvocationPaths) -> Result<u64, StoreError> {
    let mut size = 0_u64;
    for path in [
        &paths.details,
        &paths.stdout,
        &paths.stderr,
        &paths.evidence,
        &paths.bep,
        &paths.artifacts,
        &paths.test_logs_raw,
        &paths.test_log_evidence,
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
        let (mut durable, _) = read_durable(&paths.manifest).await?;
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
        let full_record = durable.invocation.clone();
        write_details(&paths, &full_record).await?;
        durable.invocation = compact_record(&full_record);
        let outcome = persist_durable(&paths, &mut durable, true).await?;
        replace_index_entry(index, id, durable.index_entry(outcome.retained_bytes));
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
    for source in [
        &paths.stdout,
        &paths.stderr,
        &paths.evidence,
        &paths.bep,
        &paths.artifacts,
        &paths.test_logs_raw,
        &paths.test_log_evidence,
    ] {
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

async fn write_details(
    paths: &InvocationPaths,
    record: &InvocationRecord,
) -> Result<(), StoreError> {
    let details = InvocationDetails::from_record(record);
    if details.targets.is_empty() && details.tests.is_empty() && details.coverage_files.is_empty() {
        remove_if_exists(&paths.details).await
    } else {
        write_json_atomic(&paths.details, &details).await
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
    T: Serialize,
    F: Fn(&T) -> String,
{
    let limit = page.limit.clamp(1, 100) as usize;
    let maximum_bytes = page.max_bytes.unwrap_or(usize::MAX);
    let invocation_id = id.to_string();
    let after = page
        .cursor
        .as_deref()
        .map(|value| OrdinalCursor::decode_for(value, scope, &invocation_id, filter))
        .transpose()?
        .map_or(-1, |cursor| cursor.ordinal);
    let normalized_filter = filter.map(str::to_ascii_lowercase);
    let total = records.len() as u64;
    let filtered = records
        .iter()
        .filter(|record| {
            normalized_filter
                .as_ref()
                .is_none_or(|filter| searchable(record).to_ascii_lowercase().contains(filter))
        })
        .count() as u64;
    let mut selected = Vec::new();
    let mut used_bytes = 2_usize;
    let mut last_ordinal = None;
    let mut truncated = false;
    for (ordinal, record) in records.into_iter().enumerate() {
        let ordinal = i64::try_from(ordinal).unwrap_or(i64::MAX);
        if ordinal <= after
            || !normalized_filter
                .as_ref()
                .is_none_or(|filter| searchable(&record).to_ascii_lowercase().contains(filter))
        {
            continue;
        }
        let item_bytes = serde_json::to_vec(&record)?.len();
        let separator = usize::from(!selected.is_empty());
        if selected.len() == limit
            || (!selected.is_empty()
                && used_bytes
                    .saturating_add(separator)
                    .saturating_add(item_bytes)
                    > maximum_bytes)
        {
            truncated = true;
            break;
        }
        used_bytes = used_bytes
            .saturating_add(separator)
            .saturating_add(item_bytes);
        last_ordinal = Some(ordinal);
        selected.push(record);
    }
    let next_cursor = if truncated {
        last_ordinal
            .map(|ordinal| OrdinalCursor::new(scope, &invocation_id, filter, ordinal).encode())
            .transpose()?
    } else {
        None
    };
    Ok((
        Page {
            items: selected,
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
    maximum_bytes: usize,
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
    if filter.is_none() {
        let total = if let Some(total) = request.known_total {
            total
        } else {
            count_query_rows(&file)?
        };
        return page_unfiltered_query_file(file, invocation_id, request, total, transform);
    }
    let mut reader = BoundedLineReader::new(BufReader::new(file), QUERY_LINE_LIMIT);
    let mut total = 0_u64;
    let mut filtered = 0_u64;
    let normalized_filter = filter.map(str::to_ascii_lowercase);
    let mut selected = Vec::with_capacity(request.limit);
    let mut used_bytes = 2_usize;
    let mut truncated = false;
    while let Some(mut line) = reader.next_line()? {
        total = total.saturating_add(1);
        line.value = transform(&line.value);
        let matches = normalized_filter
            .as_ref()
            .is_none_or(|filter| line.value.to_ascii_lowercase().contains(filter));
        if matches {
            filtered = filtered.saturating_add(1);
            if line.start_offset >= request.start_offset && !truncated {
                let item_bytes = serde_json::to_vec(&QueryRow {
                    ordinal: line.ordinal,
                    value: line.value.clone(),
                })?
                .len();
                let separator = usize::from(!selected.is_empty());
                if selected.len() == request.limit
                    || (!selected.is_empty()
                        && used_bytes
                            .saturating_add(separator)
                            .saturating_add(item_bytes)
                            > request.maximum_bytes)
                {
                    truncated = true;
                } else {
                    used_bytes = used_bytes
                        .saturating_add(separator)
                        .saturating_add(item_bytes);
                    selected.push(line);
                }
            }
        }
    }
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

fn count_query_rows(mut file: &File) -> Result<u64, StoreError> {
    use std::io::{Read, Seek, SeekFrom};

    file.seek(SeekFrom::Start(0))?;
    let mut buffer = vec![0_u8; 1024 * 1024];
    let mut rows = 0_u64;
    let mut saw_bytes = false;
    let mut last_byte = b'\n';
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        saw_bytes = true;
        last_byte = buffer[read - 1];
        rows = rows.saturating_add(
            u64::try_from(memchr::memchr_iter(b'\n', &buffer[..read]).count()).unwrap_or(u64::MAX),
        );
    }
    if saw_bytes && last_byte != b'\n' {
        rows = rows.saturating_add(1);
    }
    Ok(rows)
}

fn page_unfiltered_query_file<F>(
    mut file: File,
    invocation_id: &str,
    request: QueryFilePage,
    total: u64,
    transform: F,
) -> Result<(Page<QueryRow>, u64, u64), StoreError>
where
    F: Fn(&str) -> String,
{
    use std::io::{Seek, SeekFrom};

    file.seek(SeekFrom::Start(request.start_offset))?;
    let mut reader = BoundedLineReader::with_position(
        BufReader::new(file),
        QUERY_LINE_LIMIT,
        request.start_offset,
        request.start_ordinal,
    );
    let mut selected = Vec::with_capacity(request.limit);
    let mut used_bytes = 2_usize;
    let mut truncated = false;
    while let Some(mut line) = reader.next_line()? {
        line.value = transform(&line.value);
        let item_bytes = serde_json::to_vec(&QueryRow {
            ordinal: line.ordinal,
            value: line.value.clone(),
        })?
        .len();
        let separator = usize::from(!selected.is_empty());
        if selected.len() == request.limit
            || (!selected.is_empty()
                && used_bytes
                    .saturating_add(separator)
                    .saturating_add(item_bytes)
                    > request.maximum_bytes)
        {
            truncated = true;
            break;
        }
        used_bytes = used_bytes
            .saturating_add(separator)
            .saturating_add(item_bytes);
        selected.push(line);
    }
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
            record: compact_record(&record),
            deferred: None,
            retained_bytes: 0,
            telemetry_generation: 0,
            telemetry_flush_scheduled: false,
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
    use std::{
        io::Write as _,
        sync::atomic::{AtomicUsize, Ordering as AtomicOrdering},
    };

    use bazel_mcp_types::{
        BazelCommand, CoverageSummary, InvocationRequest, TargetCounts, TestCounts, TestStatus,
    };
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
    async fn restart_recovers_every_nonterminal_lifecycle_state() {
        let root = TempDir::new().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let queued = record(root.path());
        let queued_id = queued.request.id;
        store.create_invocation(&queued).await.unwrap();
        let starting = record(root.path());
        let starting_id = starting.request.id;
        store.create_invocation(&starting).await.unwrap();
        store
            .transition(starting_id, InvocationState::Starting, None, None)
            .await
            .unwrap();
        let running = record(root.path());
        let running_id = running.request.id;
        store.create_invocation(&running).await.unwrap();
        store
            .transition(running_id, InvocationState::Starting, None, None)
            .await
            .unwrap();
        store
            .transition(running_id, InvocationState::Running, None, None)
            .await
            .unwrap();
        drop(store);

        let reopened = Store::open(root.path()).await.unwrap();
        for id in [queued_id, starting_id, running_id] {
            let recovered = reopened.get_invocation(id).await.unwrap();
            assert_eq!(recovered.state, InvocationState::Interrupted);
            assert_eq!(recovered.termination, Some(Termination::Interrupted));
            assert!(recovered.summary.is_some());
        }
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
                    max_bytes: None,
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
                    max_bytes: None,
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
                    max_bytes: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(second.0.items[0].ordinal, 2);
    }

    #[tokio::test]
    async fn serialized_byte_packing_preserves_limit_filter_and_cursor_continuity() {
        let root = TempDir::new().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let record = record(root.path());
        let id = record.request.id;
        let paths = store.create_invocation(&record).await.unwrap();
        let contents = (0..5)
            .map(|ordinal| format!("ROW_{ordinal}_{}\n", "x".repeat(80)))
            .collect::<String>();
        tokio::fs::write(&paths.stdout, contents).await.unwrap();

        let mut cursor = None;
        let mut ordinals = Vec::new();
        loop {
            let (page, _, _) = store
                .page_query_rows(
                    id,
                    None,
                    PageRequest {
                        cursor,
                        limit: 100,
                        max_bytes: Some(120),
                    },
                )
                .await
                .unwrap();
            assert_eq!(page.items.len(), 1);
            ordinals.extend(page.items.iter().map(|row| row.ordinal));
            cursor = page.next_cursor;
            if !page.truncated {
                break;
            }
        }
        assert_eq!(ordinals, vec![0, 1, 2, 3, 4]);

        let (requested, _, _) = store
            .page_query_rows(
                id,
                None,
                PageRequest {
                    cursor: None,
                    limit: 3,
                    max_bytes: Some(8 * 1024),
                },
            )
            .await
            .unwrap();
        assert_eq!(requested.items.len(), 3);
        assert!(requested.truncated);

        let (filtered, _, filtered_count) = store
            .page_query_rows(
                id,
                Some("row_3"),
                PageRequest {
                    cursor: None,
                    limit: 100,
                    max_bytes: Some(8 * 1024),
                },
            )
            .await
            .unwrap();
        assert_eq!(filtered_count, 1);
        assert_eq!(filtered.items[0].ordinal, 3);
    }

    #[tokio::test]
    async fn unfiltered_query_count_decodes_only_the_returned_page() {
        let root = TempDir::new().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let record = record(root.path());
        let id = record.request.id;
        let paths = store.create_invocation(&record).await.unwrap();
        let contents = (0..100)
            .map(|ordinal| format!("row-{ordinal}\n"))
            .collect::<String>();
        tokio::fs::write(&paths.stdout, contents).await.unwrap();
        let transformed = Arc::new(AtomicUsize::new(0));
        let observed = transformed.clone();
        let (page, total, filtered) = store
            .page_query_rows_mapped(
                id,
                None,
                PageRequest {
                    cursor: None,
                    limit: 3,
                    max_bytes: None,
                },
                move |value| {
                    observed.fetch_add(1, AtomicOrdering::Relaxed);
                    value.to_owned()
                },
            )
            .await
            .unwrap();
        assert_eq!((total, filtered), (100, 100));
        assert_eq!(page.items.len(), 3);
        assert_eq!(transformed.load(AtomicOrdering::Relaxed), 4);
    }

    #[tokio::test]
    async fn query_count_handles_crlf_invalid_utf8_and_unterminated_rows() {
        let root = TempDir::new().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let record = record(root.path());
        let id = record.request.id;
        let paths = store.create_invocation(&record).await.unwrap();
        tokio::fs::write(&paths.stdout, b"first\r\ninvalid-\xff\nlast")
            .await
            .unwrap();
        let (first, total, filtered) = store
            .page_query_rows(
                id,
                None,
                PageRequest {
                    cursor: None,
                    limit: 2,
                    max_bytes: None,
                },
            )
            .await
            .unwrap();
        assert_eq!((total, filtered), (3, 3));
        assert_eq!(first.items[0].value, "first");
        assert_eq!(first.items[1].value, "invalid-�");
        let (last, _, _) = store
            .page_query_rows(
                id,
                None,
                PageRequest {
                    cursor: first.next_cursor,
                    limit: 2,
                    max_bytes: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(last.items[0].value, "last");
        assert_eq!(last.items[0].ordinal, 2);
    }

    #[tokio::test]
    async fn canonical_manifest_excludes_large_details_and_survives_restart() {
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
        let summary = InvocationSummary {
            success: true,
            headline: "complete".into(),
            targets: vec![TargetResult {
                label: "//private:large-target-detail".into(),
                success: true,
            }],
            target_counts: TargetCounts {
                requested: 1,
                succeeded: 1,
                failed: 0,
            },
            tests: vec![TestResult {
                label: "//private:large-test-detail".into(),
                status: TestStatus::Passed,
                duration_ms: Some(1),
                attempts: 1,
                shard: None,
                cases: Vec::new(),
                test_log_available: false,
                test_log_unavailable_reason: None,
            }],
            test_counts: TestCounts {
                passed: 1,
                ..TestCounts::default()
            },
            coverage: Some(CoverageSummary {
                lines_found: 1,
                lines_hit: 1,
                coverage_percent: 100.0,
                files: vec![CoverageFile {
                    path: "private/large-coverage-detail.rs".into(),
                    lines_found: 1,
                    lines_hit: 1,
                    coverage_percent: 100.0,
                }],
            }),
            ..InvocationSummary::default()
        };
        store
            .finish_invocation(
                id,
                InvocationCompletion {
                    state: InvocationState::Succeeded,
                    termination: Termination::Exit { code: 0 },
                    summary,
                    metrics: InvocationMetrics::default(),
                    canonical_arguments: None,
                    artifacts: Vec::new(),
                },
            )
            .await
            .unwrap();
        let manifest = tokio::fs::read_to_string(&paths.manifest).await.unwrap();
        assert_eq!(manifest.matches("\"request\"").count(), 1);
        assert_eq!(manifest.matches("\"summary\"").count(), 1);
        assert!(!manifest.contains("large-target-detail"));
        assert!(!manifest.contains("large-test-detail"));
        assert!(!manifest.contains("large-coverage-detail"));
        assert!(!paths.directory.join("request.json").exists());
        assert!(!paths.directory.join("summary.json").exists());
        assert!(!paths.artifacts.exists());
        drop(store);

        let reopened = Store::open(root.path()).await.unwrap();
        assert_eq!(
            reopened
                .page_tests(id, None, PageRequest::default())
                .await
                .unwrap()
                .0
                .items[0]
                .label,
            "//private:large-test-detail"
        );
        assert_eq!(
            reopened
                .page_coverage(id, None, PageRequest::default())
                .await
                .unwrap()
                .0
                .items[0]
                .path,
            "private/large-coverage-detail.rs"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn canonical_directories_and_files_remain_private() {
        use std::os::unix::fs::PermissionsExt;

        let root = TempDir::new().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let record = record(root.path());
        let paths = store.create_invocation(&record).await.unwrap();
        assert_eq!(
            std::fs::metadata(&paths.directory)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&paths.manifest)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[tokio::test]
    async fn invocation_locks_serialize_one_id_without_blocking_another() {
        let root = TempDir::new().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let first = record(root.path());
        let first_id = first.request.id;
        store.create_invocation(&first).await.unwrap();
        let second = record(root.path());
        let second_id = second.request.id;
        store.create_invocation(&second).await.unwrap();

        let first_lock = store.mutation_lock(first_id).await;
        let guard = first_lock.lock().await;
        let blocked_store = store.clone();
        let blocked = tokio::spawn(async move {
            blocked_store
                .transition(first_id, InvocationState::Starting, None, None)
                .await
        });
        let independent_store = store.clone();
        let independent = tokio::spawn(async move {
            independent_store
                .transition(second_id, InvocationState::Starting, None, None)
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), independent)
            .await
            .expect("independent invocation was blocked by another invocation's lock")
            .unwrap()
            .unwrap();
        assert!(!blocked.is_finished());
        drop(guard);
        blocked.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn racing_terminal_mutations_cannot_lose_or_regress_state() {
        let root = TempDir::new().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let record = record(root.path());
        let id = record.request.id;
        store.create_invocation(&record).await.unwrap();
        store
            .transition(id, InvocationState::Starting, None, None)
            .await
            .unwrap();
        store
            .transition(id, InvocationState::Running, None, None)
            .await
            .unwrap();
        let succeeded = {
            let store = store.clone();
            tokio::spawn(async move {
                store
                    .transition(
                        id,
                        InvocationState::Succeeded,
                        Some(Termination::Exit { code: 0 }),
                        Some(InvocationSummary::default()),
                    )
                    .await
            })
        };
        let cancelled = {
            let store = store.clone();
            tokio::spawn(async move {
                store
                    .transition(
                        id,
                        InvocationState::Cancelled,
                        Some(Termination::Cancelled),
                        Some(InvocationSummary::default()),
                    )
                    .await
            })
        };
        let outcomes = [succeeded.await.unwrap(), cancelled.await.unwrap()];
        assert_eq!(outcomes.iter().filter(|outcome| outcome.is_ok()).count(), 1);
        assert!(store.get_invocation(id).await.unwrap().state.is_terminal());
    }

    #[tokio::test]
    async fn telemetry_updates_are_coalesced_and_eventually_durable() {
        let root = TempDir::new().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let record = record(root.path());
        let id = record.request.id;
        store.create_invocation(&record).await.unwrap();
        succeed(&store, id).await;
        let before = store.io_stats();
        for _ in 0..100 {
            store
                .record_model_visible_result(id, 10, true)
                .await
                .unwrap();
        }
        store.record_progress_notifications(id, 7).await.unwrap();
        assert_eq!(store.io_stats().manifest_commits, before.manifest_commits);
        tokio::time::sleep(Duration::from_millis(400)).await;
        let after = store.io_stats();
        assert_eq!(after.manifest_commits, before.manifest_commits + 1);
        let current = store.get_invocation(id).await.unwrap();
        assert_eq!(current.metrics.model_visible_bytes, 1_000);
        assert_eq!(current.metrics.inspect_calls, 100);
        assert_eq!(current.metrics.progress_notifications, 7);
        drop(store);
        let reopened = Store::open(root.path()).await.unwrap();
        let durable = reopened.get_invocation(id).await.unwrap();
        assert_eq!(durable.metrics.model_visible_bytes, 1_000);
        assert_eq!(durable.metrics.inspect_calls, 100);
        assert_eq!(durable.metrics.progress_notifications, 7);
    }

    #[tokio::test]
    async fn terminal_commit_absorbs_pending_telemetry_flush() {
        let root = TempDir::new().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let record = record(root.path());
        let id = record.request.id;
        store.create_invocation(&record).await.unwrap();
        store
            .transition(id, InvocationState::Starting, None, None)
            .await
            .unwrap();
        store
            .transition(id, InvocationState::Running, None, None)
            .await
            .unwrap();
        store.record_progress_notifications(id, 3).await.unwrap();
        let before = store.io_stats().manifest_commits;
        store
            .transition(
                id,
                InvocationState::Succeeded,
                Some(Termination::Exit { code: 0 }),
                Some(InvocationSummary::default()),
            )
            .await
            .unwrap();
        assert_eq!(store.io_stats().manifest_commits, before + 1);
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(store.io_stats().manifest_commits, before + 1);
        drop(store);
        let reopened = Store::open(root.path()).await.unwrap();
        assert_eq!(
            reopened
                .get_invocation(id)
                .await
                .unwrap()
                .metrics
                .progress_notifications,
            3
        );
    }

    #[tokio::test]
    async fn gc_waiting_on_one_invocation_does_not_block_index_lookups() {
        let root = TempDir::new().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let first = record(root.path());
        let first_id = first.request.id;
        let first_paths = store.create_invocation(&first).await.unwrap();
        tokio::fs::write(&first_paths.stdout, vec![b'x'; 4096])
            .await
            .unwrap();
        succeed(&store, first_id).await;
        tokio::time::sleep(Duration::from_millis(2)).await;
        let second = record(root.path());
        let second_id = second.request.id;
        store.create_invocation(&second).await.unwrap();
        succeed(&store, second_id).await;

        let first_lock = store.mutation_lock(first_id).await;
        let guard = first_lock.lock().await;
        let gc_store = store.clone();
        let gc =
            tokio::spawn(
                async move { gc_store.enforce_retention(Duration::from_secs(0), 1).await },
            );
        tokio::task::yield_now().await;
        tokio::time::timeout(Duration::from_secs(1), store.get_invocation(second_id))
            .await
            .expect("GC held the shared index while waiting on another invocation")
            .unwrap();
        drop(guard);
        gc.await.unwrap().unwrap();
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

        tokio::fs::write(paths.manifest.with_extension("tmp"), b"truncated")
            .await
            .unwrap();
        tokio::fs::write(paths.details.with_extension("tmp"), b"truncated")
            .await
            .unwrap();
        let reopened = Store::open(root.path()).await.unwrap();
        assert_eq!(
            reopened.get_invocation(id).await.unwrap().state,
            InvocationState::Succeeded
        );
        assert!(!paths.manifest.with_extension("tmp").exists());
        assert!(!paths.details.with_extension("tmp").exists());
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
            .mutate(id, false, |durable| {
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
    async fn retention_removes_invocation_owned_failed_test_snapshots() {
        let root = TempDir::new().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let record = InvocationRecord::queued(InvocationRequest::new(
            root.path().to_owned(),
            BazelCommand::Test,
            vec!["//pkg:failing".into()],
        ));
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
        tokio::fs::write(&paths.test_logs_raw, b"complete private failure log")
            .await
            .unwrap();
        tokio::fs::write(
            &paths.test_log_evidence,
            b"{\"label\":\"//pkg:failing\",\"text\":\"assertion failed\"}\n",
        )
        .await
        .unwrap();
        store
            .transition(
                id,
                InvocationState::Failed,
                Some(Termination::Exit { code: 1 }),
                Some(InvocationSummary::default()),
            )
            .await
            .unwrap();

        assert_eq!(
            store
                .enforce_retention(Duration::ZERO, u64::MAX)
                .await
                .unwrap(),
            1
        );
        assert!(!paths.directory.exists());
        assert!(matches!(
            store.get_invocation(id).await,
            Err(StoreError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn corrupt_committed_records_fail_closed_without_overwriting_evidence() {
        let root = TempDir::new().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let record = record(root.path());
        let paths = store.create_invocation(&record).await.unwrap();
        drop(store);
        let corrupt = b"{not-json";
        tokio::fs::write(&paths.manifest, corrupt).await.unwrap();
        assert!(matches!(
            Store::open(root.path()).await,
            Err(StoreError::CorruptRecord { .. })
        ));
        assert_eq!(tokio::fs::read(paths.manifest).await.unwrap(), corrupt);
    }

    #[tokio::test]
    async fn corrupt_detail_sidecars_fail_inspection_without_damaging_manifest() {
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
        store
            .finish_invocation(
                id,
                InvocationCompletion {
                    state: InvocationState::Succeeded,
                    termination: Termination::Exit { code: 0 },
                    summary: InvocationSummary {
                        tests: vec![TestResult {
                            label: "//test:one".into(),
                            status: TestStatus::Passed,
                            duration_ms: None,
                            attempts: 1,
                            shard: None,
                            cases: Vec::new(),
                            test_log_available: false,
                            test_log_unavailable_reason: None,
                        }],
                        ..InvocationSummary::default()
                    },
                    metrics: InvocationMetrics::default(),
                    canonical_arguments: None,
                    artifacts: Vec::new(),
                },
            )
            .await
            .unwrap();
        let manifest = tokio::fs::read(&paths.manifest).await.unwrap();
        tokio::fs::write(&paths.details, b"{not-json")
            .await
            .unwrap();
        assert!(matches!(
            store.page_tests(id, None, PageRequest::default()).await,
            Err(StoreError::Json(_))
        ));
        assert_eq!(tokio::fs::read(&paths.manifest).await.unwrap(), manifest);
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

    #[tokio::test]
    async fn query_reader_handles_empty_and_million_row_files() {
        let root = TempDir::new().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let empty = record(root.path());
        let empty_id = empty.request.id;
        store.create_invocation(&empty).await.unwrap();
        let (page, total, filtered) = store
            .page_query_rows(empty_id, None, PageRequest::default())
            .await
            .unwrap();
        assert!(page.items.is_empty());
        assert_eq!((total, filtered), (0, 0));

        let million = record(root.path());
        let million_id = million.request.id;
        let paths = store.create_invocation(&million).await.unwrap();
        let mut writer = std::io::BufWriter::new(std::fs::File::create(&paths.stdout).unwrap());
        for _ in 0..1_000_000 {
            writer.write_all(b"row\n").unwrap();
        }
        writer.flush().unwrap();
        let (page, total, filtered) = store
            .page_query_rows(
                million_id,
                None,
                PageRequest {
                    cursor: None,
                    limit: 3,
                    max_bytes: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(page.items.len(), 3);
        assert_eq!((total, filtered), (1_000_000, 1_000_000));
    }

    #[tokio::test]
    async fn failed_gc_unlink_leaves_recoverable_trash_without_reindexing() {
        let root = TempDir::new().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let record = record(root.path());
        let id = record.request.id;
        store.create_invocation(&record).await.unwrap();
        succeed(&store, id).await;
        store
            .inner
            .fail_next_gc_unlink
            .store(true, Ordering::Relaxed);

        assert_eq!(
            store
                .enforce_retention(Duration::from_secs(0), 0)
                .await
                .unwrap(),
            1
        );
        assert!(matches!(
            store.get_invocation(id).await,
            Err(StoreError::NotFound(_))
        ));
        let trash = root.path().join("trash").join(id.to_string());
        assert!(trash.exists());
        drop(store);

        let reopened = Store::open(root.path()).await.unwrap();
        assert!(!trash.exists());
        assert!(matches!(
            reopened.get_invocation(id).await,
            Err(StoreError::NotFound(_))
        ));
    }
}
