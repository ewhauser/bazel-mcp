use std::{
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

#[cfg(test)]
use bazel_mcp_types::{DeferredRetrieval, TargetResult};

use bazel_mcp_types::{
    Artifact, CoverageFile, DeferredResultRecord, Diagnostic, InvocationId, InvocationMetrics,
    InvocationRecord, InvocationState, InvocationSummary, Page, PageRequest, QueryRow, Termination,
    TestResult,
};
use serde::de::DeserializeOwned;
use thiserror::Error;
use tokio::sync::Mutex;

use crate::{
    InvocationPaths,
    coordination::{ChangeCoordinator, LeaseManager, ProcessLock, mutation_lock_path},
    cursor::FileCursor,
    files::{
        create_private_directory, create_private_directory_all, remove_if_exists, write_json_atomic,
    },
    index::{
        Index, IndexEntry, ensure_exists, insert as insert_index_entry, mark_telemetry_flushed,
        merge_index_telemetry, replace as replace_index_entry,
    },
    index_coordinator::IndexCoordinator,
    manifest::{CURRENT_SCHEMA_VERSION, DurableRecord, read as read_durable},
    manifest_repository::ManifestRepository,
    metrics::StoreMetrics,
    query_paging::{QueryFilePage, count_query_file, page_query_file, page_records},
    record::{HydratedInvocation, InvocationDetails, InvocationHeader},
    recovery::{RecoveryManager, nonterminal_ids},
};

#[cfg(test)]
use crate::{
    coordination::{owner_lock_path, read_changes},
    manifest_repository::persist as persist_manifest,
    query_paging::QUERY_LINE_LIMIT,
    record::InvocationSummaryHeader,
};

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("invocation was not found: {0}")]
    NotFound(InvocationId),
    #[error("deferred result was not found or has expired: {0}")]
    DeferredNotFound(InvocationId),
    #[error("invalid pagination cursor")]
    InvalidCursor,
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
    pub(crate) cache_root: PathBuf,
    pub(crate) inner: Arc<StoreInner>,
}

pub(crate) struct StoreInner {
    pub(crate) index: IndexCoordinator,
    pub(crate) manifests: ManifestRepository,
    pub(crate) leases: LeaseManager,
    pub(crate) changes: ChangeCoordinator,
    pub(crate) metrics: StoreMetrics,
    startup_stats: StoreStartupStats,
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

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ReclaimOutcome {
    pub(crate) changed: bool,
    pub(crate) deleted: bool,
    pub(crate) reclaimed: bool,
}

/// One coalesced terminal metadata commit for a completed Bazel invocation.
pub struct InvocationCompletion {
    pub state: InvocationState,
    pub termination: Termination,
    pub summary: InvocationSummary,
    pub run: Option<bazel_mcp_types::RunSummary>,
    pub metrics: InvocationMetrics,
    pub canonical_arguments: Option<Vec<String>>,
    pub artifacts: Vec<Artifact>,
}

impl Store {
    pub async fn open(cache_root: impl AsRef<Path>) -> Result<Self, StoreError> {
        let cache_root = cache_root.as_ref().to_owned();
        create_private_directory_all(&cache_root).await?;
        create_private_directory_all(&cache_root.join("invocations")).await?;
        create_private_directory_all(&cache_root.join("trash")).await?;
        create_private_directory_all(&cache_root.join("owners")).await?;
        create_private_directory_all(&cache_root.join("mutations")).await?;
        create_private_directory_all(&cache_root.join("changes")).await?;

        let _maintenance = ProcessLock::acquire(cache_root.join("MAINTENANCE")).await?;
        let changes = ChangeCoordinator::open(&cache_root).await?;
        RecoveryManager::clean_trash(&cache_root).await?;
        let (mut index, startup_stats) = RecoveryManager::load_index(&cache_root, true).await?;
        let recovered = RecoveryManager::recover_interrupted(&cache_root, &mut index).await?;
        if recovered != 0 {
            changes.publish().await?;
        }

        Ok(Self {
            cache_root: cache_root.clone(),
            inner: Arc::new(StoreInner {
                index: IndexCoordinator::new(index),
                manifests: ManifestRepository::new(cache_root.clone()),
                leases: LeaseManager::new(),
                changes,
                metrics: StoreMetrics::new(),
                startup_stats,
            }),
        })
    }

    #[must_use]
    pub fn paths_for(&self, record: &InvocationRecord) -> InvocationPaths {
        self.inner.manifests.paths_for_id(record.request.id)
    }

    pub(crate) fn paths_for_id(&self, id: InvocationId) -> InvocationPaths {
        self.inner.manifests.paths_for_id(id)
    }

    pub(crate) async fn mutation_lock(&self, id: InvocationId) -> Arc<Mutex<()>> {
        self.inner.leases.mutation_lock(id).await
    }

    async fn acquire_owner(&self, id: InvocationId) -> Result<ProcessLock, StoreError> {
        self.inner.leases.acquire_owner(&self.cache_root, id).await
    }

    async fn release_owner(&self, id: InvocationId) {
        self.inner.leases.release_owner(id).await;
    }

    pub(crate) async fn recover_orphaned_invocations(&self) -> Result<usize, StoreError> {
        let ids = {
            let index = self.inner.index.read().await;
            nonterminal_ids(&index)
        };
        let recovered = RecoveryManager::recover_ids(&self.cache_root, ids).await?;
        if !recovered.is_empty() {
            let recovered_count = recovered.len();
            {
                let mut index = self.inner.index.write().await;
                for (id, entry) in recovered {
                    replace_index_entry(&mut index, id, entry);
                }
            }
            self.publish_change().await?;
            return Ok(recovered_count);
        }
        Ok(0)
    }

    pub(crate) async fn publish_change(&self) -> Result<(), StoreError> {
        self.inner.changes.publish().await
    }

    pub(crate) async fn refresh_index_if_stale(&self) -> Result<(), StoreError> {
        if !self.claim_change_check() {
            return Ok(());
        }
        self.refresh_index_if_changed().await
    }

    async fn refresh_index_if_changed(&self) -> Result<(), StoreError> {
        self.refresh_index(false).await
    }

    fn claim_change_check(&self) -> bool {
        self.inner.changes.claim_check(monotonic_us())
    }

    pub(crate) async fn refresh_index(&self, force: bool) -> Result<(), StoreError> {
        let Some(refresh) = self
            .inner
            .changes
            .begin_refresh(&self.cache_root, force)
            .await?
        else {
            return Ok(());
        };
        let (refreshed, _) = RecoveryManager::load_index(&self.cache_root, false).await?;
        self.inner.index.replace_from_disk(refreshed).await;
        refresh.commit();
        Ok(())
    }

    #[must_use]
    pub fn io_stats(&self) -> StoreIoStats {
        self.inner.metrics.snapshot()
    }

    #[must_use]
    pub fn startup_stats(&self) -> StoreStartupStats {
        self.inner.startup_stats
    }

    pub(crate) async fn persist_durable(
        &self,
        paths: &InvocationPaths,
        durable: &mut DurableRecord,
        recount_payload: bool,
    ) -> Result<u64, StoreError> {
        self.inner.metrics.record_manifest_commit(recount_payload);
        let outcome = self
            .inner
            .manifests
            .persist(paths, durable, recount_payload)
            .await?;
        self.inner
            .metrics
            .record_manifest_bytes(outcome.manifest_bytes);
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
        self.refresh_index_if_stale().await?;
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
        let owner = if record.state.is_terminal() {
            None
        } else {
            Some(self.acquire_owner(id).await?)
        };
        let _process_mutation =
            ProcessLock::acquire(mutation_lock_path(&self.cache_root, id)).await?;
        paths.create().await?;
        let result = async {
            let mut durable = DurableRecord {
                schema_version: CURRENT_SCHEMA_VERSION,
                invocation: InvocationHeader::from_record(record),
                deferred: deferred.cloned(),
                payload_bytes: 0,
            };
            let retained_bytes = self.persist_durable(&paths, &mut durable, true).await?;
            self.publish_change().await?;
            Ok::<_, StoreError>((durable, retained_bytes))
        }
        .await;
        let (durable, retained_bytes) = match result {
            Ok(durable) => durable,
            Err(error) => {
                let _ = tokio::fs::remove_dir_all(&paths.directory).await;
                drop(owner);
                return Err(error);
            }
        };
        let mut index = self.inner.index.write().await;
        insert_index_entry(&mut index, id, durable.index_entry(retained_bytes));
        drop(index);
        if let Some(owner) = owner {
            self.inner.leases.retain_owner(id, owner).await;
        }
        Ok(paths)
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
        let _process_mutation =
            ProcessLock::acquire(mutation_lock_path(&self.cache_root, id)).await?;
        let paths = self.paths_for_id(id);
        let (mut durable, _) = match read_durable(&paths.manifest).await {
            Ok(durable) => durable,
            Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut index = self.inner.index.write().await;
                let result = transition_lost_evidence(&mut index, id, next, termination, summary);
                drop(index);
                if next.is_terminal() {
                    self.release_owner(id).await;
                }
                return result;
            }
            Err(error) => return Err(error),
        };
        let telemetry_generation = {
            let index = self.inner.index.read().await;
            merge_index_telemetry(&index, id, &mut durable.invocation.metrics)
        };
        let mut result = durable.invocation.clone().into_record();
        result.transition(next)?;
        result.termination = termination.clone();
        result.summary = summary.clone();
        if next.is_terminal()
            && let Some(deferred) = durable.deferred.as_mut()
        {
            let terminal_at_ms = result
                .finished_at_ms
                .unwrap_or_else(bazel_mcp_types::unix_timestamp_ms);
            deferred.extend_terminal_expiry(terminal_at_ms);
        }
        durable.invocation = InvocationHeader::from_record(&result);
        if next.is_terminal() {
            write_details(&paths, &result).await?;
        }
        let retained_bytes = match self
            .persist_durable(&paths, &mut durable, next.is_terminal())
            .await
        {
            Ok(retained_bytes) => {
                self.publish_change().await?;
                retained_bytes
            }
            Err(error) if error_is_not_found(&error) => {
                let mut index = self.inner.index.write().await;
                let result = transition_lost_evidence(&mut index, id, next, termination, summary);
                drop(index);
                if next.is_terminal() {
                    self.release_owner(id).await;
                }
                return result;
            }
            Err(error) => return Err(error),
        };
        let mut index = self.inner.index.write().await;
        replace_index_entry(&mut index, id, durable.index_entry(retained_bytes));
        mark_telemetry_flushed(&mut index, id, telemetry_generation);
        drop(index);
        if next.is_terminal() {
            self.release_owner(id).await;
        }
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
            run,
            metrics,
            canonical_arguments,
            artifacts,
        } = completion;
        if !state.is_terminal() {
            return Err(StoreError::State(bazel_mcp_types::StateTransitionError {
                current: self.get_invocation_header(id).await?.state,
                next: state,
            }));
        }
        let mutation_lock = self.mutation_lock(id).await;
        let _guard = mutation_lock.lock().await;
        self.ensure_invocation(id).await?;
        let _process_mutation =
            ProcessLock::acquire(mutation_lock_path(&self.cache_root, id)).await?;
        let paths = self.paths_for_id(id);
        let (mut durable, _) = read_durable(&paths.manifest).await?;
        let mut result = durable.invocation.clone().into_record();
        result.transition(state)?;
        result.termination = Some(termination);
        result.summary = Some(summary);
        result.run = run;
        result.metrics = metrics;
        let telemetry_generation = {
            let index = self.inner.index.read().await;
            merge_index_telemetry(&index, id, &mut result.metrics)
        };
        result.canonical_arguments = canonical_arguments;
        if let Some(deferred) = durable.deferred.as_mut() {
            deferred.extend_terminal_expiry(
                result
                    .finished_at_ms
                    .unwrap_or_else(bazel_mcp_types::unix_timestamp_ms),
            );
        }
        durable.invocation = InvocationHeader::from_record(&result);
        if artifacts.is_empty() {
            remove_if_exists(&paths.artifacts).await?;
        } else {
            write_json_atomic(&paths.artifacts, &artifacts).await?;
        }
        write_details(&paths, &result).await?;
        let retained_bytes = self.persist_durable(&paths, &mut durable, true).await?;
        self.publish_change().await?;
        let mut index = self.inner.index.write().await;
        replace_index_entry(&mut index, id, durable.index_entry(retained_bytes));
        mark_telemetry_flushed(&mut index, id, telemetry_generation);
        drop(index);
        self.release_owner(id).await;
        Ok(result)
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
        let _process_mutation =
            ProcessLock::acquire(mutation_lock_path(&self.cache_root, id)).await?;
        let paths = self.paths_for_id(id);
        if artifacts.is_empty() {
            remove_if_exists(&paths.artifacts).await?;
        } else {
            write_json_atomic(&paths.artifacts, artifacts).await?;
        }
        let (mut durable, _) = read_durable(&paths.manifest).await?;
        let retained_bytes = self.persist_durable(&paths, &mut durable, true).await?;
        self.publish_change().await?;
        let mut index = self.inner.index.write().await;
        replace_index_entry(&mut index, id, durable.index_entry(retained_bytes));
        Ok(())
    }

    pub async fn page_diagnostics(
        &self,
        id: InvocationId,
        filter: Option<&str>,
        page: PageRequest,
    ) -> Result<Page<Diagnostic>, StoreError> {
        let record = self.get_invocation_header(id).await?;
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
    ) -> Result<Page<TestResult>, StoreError> {
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
    ) -> Result<Page<Artifact>, StoreError> {
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
    ) -> Result<Page<CoverageFile>, StoreError> {
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
    ) -> Result<Page<QueryRow>, StoreError> {
        self.page_query_rows_mapped_into(id, filter, page, |value, output| {
            output.clear();
            output.push_str(value);
        })
        .await
    }

    /// Count newline-delimited query results without decoding or materializing rows.
    pub async fn count_query_rows(&self, id: InvocationId) -> Result<u64, StoreError> {
        let mutation_lock = self.mutation_lock(id).await;
        let _guard = mutation_lock.lock().await;
        self.ensure_invocation(id).await?;
        let path = self.paths_for_id(id).stdout;
        tokio::task::spawn_blocking(move || count_query_file(&path)).await?
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
    ) -> Result<Page<QueryRow>, StoreError>
    where
        F: Fn(&str) -> String + Send + 'static,
    {
        self.page_query_rows_mapped_into(id, filter, page, move |value, output| {
            *output = transform(value);
        })
        .await
    }

    /// Page raw query output while reusing a caller-populated transformation
    /// buffer across scanned rows.
    pub async fn page_query_rows_mapped_into<F>(
        &self,
        id: InvocationId,
        filter: Option<&str>,
        page: PageRequest,
        transform: F,
    ) -> Result<Page<QueryRow>, StoreError>
    where
        F: Fn(&str, &mut String) + Send + 'static,
    {
        let mutation_lock = self.mutation_lock(id).await;
        let _guard = mutation_lock.lock().await;
        self.ensure_invocation(id).await?;
        let known_total = {
            let index = self.inner.index.read().await;
            let entry = index.entries.get(&id).ok_or(StoreError::NotFound(id))?;
            (filter.is_none() && entry.record.state.is_terminal())
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
        let prior_total = cursor.as_ref().map_or(0, |value| value.total_scanned);
        let prior_filtered = cursor.as_ref().map_or(0, |value| value.filtered_scanned);
        let path = self.paths_for_id(id).stdout;
        let filter = filter.map(str::to_owned);
        let item_limit = page.item_limit.clamp(1, 100) as usize;
        let scan_limit = page.scan_limit.clamp(page.item_limit.max(1), 10_000) as usize;
        tokio::task::spawn_blocking(move || {
            page_query_file(
                &path,
                &invocation_id,
                filter.as_deref(),
                QueryFilePage {
                    start_offset,
                    start_ordinal,
                    prior_total,
                    prior_filtered,
                    item_limit,
                    scan_limit,
                    known_total,
                },
                transform,
            )
        })
        .await?
    }

    pub(crate) async fn mutate<F>(
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
        let _process_mutation =
            ProcessLock::acquire(mutation_lock_path(&self.cache_root, id)).await?;
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
        self.publish_change().await?;
        let mut index = self.inner.index.write().await;
        replace_index_entry(&mut index, id, durable.index_entry(retained_bytes));
        mark_telemetry_flushed(&mut index, id, telemetry_generation);
        Ok(())
    }

    pub(crate) async fn ensure_invocation(&self, id: InvocationId) -> Result<(), StoreError> {
        self.refresh_index_if_stale().await?;
        if self.inner.index.read().await.entries.contains_key(&id) {
            return Ok(());
        }
        // A miss is uncommon and must not wait for the coalescing interval:
        // another process may have just committed this invocation.
        self.refresh_index_if_changed().await?;
        let index = self.inner.index.read().await;
        ensure_exists(&index, id)
    }

    pub(crate) async fn read_hydrated_invocation(
        &self,
        id: InvocationId,
    ) -> Result<HydratedInvocation, StoreError> {
        let mutation_lock = self.mutation_lock(id).await;
        let _guard = mutation_lock.lock().await;
        self.ensure_invocation(id).await?;
        let paths = self.paths_for_id(id);
        let (durable, _) = read_durable(&paths.manifest).await?;
        let details = read_json_or_default::<InvocationDetails>(&paths.details).await?;
        Ok(HydratedInvocation {
            header: durable.invocation,
            details,
        })
    }

    async fn read_details(&self, id: InvocationId) -> Result<InvocationDetails, StoreError> {
        let mutation_lock = self.mutation_lock(id).await;
        let _guard = mutation_lock.lock().await;
        self.ensure_invocation(id).await?;
        read_json_or_default(&self.paths_for_id(id).details).await
    }
}

pub(crate) fn elapsed_us(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn monotonic_us() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    elapsed_us(START.get_or_init(Instant::now).elapsed())
}

pub(crate) fn retention_age_elapsed(finished_at_ms: i64, cutoff_ms: i64) -> bool {
    finished_at_ms <= cutoff_ms
}

pub(crate) async fn rename_to_trash(
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

pub(crate) async fn stage_raw_evidence(
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
    create_private_directory(&trash).await?;
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

pub(crate) async fn finish_staged_evidence(
    cache_root: &Path,
    id: InvocationId,
) -> Result<(), StoreError> {
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
    if details.is_empty() {
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
        .clone()
        .into_record();
    record.transition(next)?;
    record.termination = termination;
    record.summary = summary;
    replace_index_entry(
        index,
        id,
        IndexEntry {
            record: InvocationHeader::from_record(&record),
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
    async fn shared_cache_accepts_concurrent_store_processes() {
        let root = TempDir::new().unwrap();
        let first = Store::open(root.path()).await.unwrap();
        let second = Store::open(root.path()).await.unwrap();
        let first_record = record(&root.path().join("worktree-a"));
        let first_id = first_record.request.id;
        first.create_invocation(&first_record).await.unwrap();
        let second_record = record(&root.path().join("worktree-b"));
        let second_id = second_record.request.id;
        second.create_invocation(&second_record).await.unwrap();

        assert_eq!(
            first.get_invocation(second_id).await.unwrap().request.id,
            second_id
        );
        assert_eq!(
            second.get_invocation(first_id).await.unwrap().request.id,
            first_id
        );
        let page = first
            .list_invocations(None, None, None, PageRequest::new(None, 10))
            .await
            .unwrap();
        assert_eq!(page.items.len(), 2);
    }

    #[tokio::test]
    async fn replacement_publisher_preserves_an_unseen_change_notification() {
        let root = TempDir::new().unwrap();
        let writer = Store::open(root.path()).await.unwrap();
        let observer = Store::open(root.path()).await.unwrap();
        let record = record(&root.path().join("worktree-a"));
        let id = record.request.id;
        writer.create_invocation(&record).await.unwrap();
        let (marker, lease) = writer.inner.changes.publisher_paths(root.path()).await;
        drop(writer);

        assert!(marker.exists());
        assert!(lease.exists());
        let _replacement = Store::open(root.path()).await.unwrap();
        assert!(!marker.exists());
        assert!(!lease.exists());
        assert_eq!(observer.get_invocation(id).await.unwrap().request.id, id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn local_refresh_cannot_discard_concurrent_creations() {
        let root = TempDir::new().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let refresher = {
            let store = store.clone();
            tokio::spawn(async move {
                for _ in 0..64 {
                    store.refresh_index(true).await.unwrap();
                }
            })
        };
        let mut writers = Vec::new();
        for ordinal in 0..64 {
            let store = store.clone();
            let workspace = root.path().join(format!("worktree-{ordinal}"));
            writers.push(tokio::spawn(async move {
                let record = record(&workspace);
                let id = record.request.id;
                store.create_invocation(&record).await.unwrap();
                store
                    .transition(id, InvocationState::Starting, None, None)
                    .await
                    .unwrap();
                id
            }));
        }
        let mut ids = Vec::new();
        for writer in writers {
            ids.push(writer.await.unwrap());
        }
        refresher.await.unwrap();
        for id in ids {
            assert_eq!(store.get_invocation(id).await.unwrap().request.id, id);
        }
    }

    #[tokio::test]
    async fn startup_recovery_skips_invocations_owned_by_another_process() {
        let root = TempDir::new().unwrap();
        let first = Store::open(root.path()).await.unwrap();
        let record = record(&root.path().join("worktree-a"));
        let id = record.request.id;
        first.create_invocation(&record).await.unwrap();

        let observer = Store::open(root.path()).await.unwrap();
        assert_eq!(
            observer.get_invocation(id).await.unwrap().state,
            InvocationState::Queued
        );

        drop(first);
        let recovery = Store::open(root.path()).await.unwrap();
        assert_eq!(
            recovery.get_invocation(id).await.unwrap().state,
            InvocationState::Interrupted
        );
        assert_eq!(
            observer.get_invocation(id).await.unwrap().state,
            InvocationState::Interrupted
        );
    }

    #[tokio::test]
    async fn global_gc_reclaims_records_created_by_another_process() {
        let root = TempDir::new().unwrap();
        let first = Store::open(root.path()).await.unwrap();
        let second = Store::open(root.path()).await.unwrap();
        let record = record(&root.path().join("worktree-a"));
        let id = record.request.id;
        first.create_invocation(&record).await.unwrap();
        succeed(&first, id).await;

        assert_eq!(
            second
                .enforce_retention(Duration::ZERO, u64::MAX)
                .await
                .unwrap(),
            1
        );
        assert!(matches!(
            first.get_invocation(id).await,
            Err(StoreError::NotFound(missing)) if missing == id
        ));
        assert!(!owner_lock_path(root.path(), id).exists());
        assert!(!mutation_lock_path(root.path(), id).exists());
    }

    #[tokio::test]
    async fn maintenance_repairs_a_manifest_committed_before_change_notification() {
        let root = TempDir::new().unwrap();
        let writer = Store::open(root.path()).await.unwrap();
        let observer = Store::open(root.path()).await.unwrap();
        let record = record(&root.path().join("worktree-a"));
        let id = record.request.id;
        let paths = writer.create_invocation(&record).await.unwrap();
        observer.get_invocation(id).await.unwrap();
        let changes = read_changes(root.path()).unwrap();

        // Simulate a process dying after its atomic manifest rename but before
        // it can publish the change notification.
        let (mut durable, _) = read_durable(&paths.manifest).await.unwrap();
        durable.invocation.state = InvocationState::Succeeded;
        durable.invocation.finished_at_ms = Some(bazel_mcp_types::unix_timestamp_ms() - 1);
        durable.invocation.termination = Some(Termination::Exit { code: 0 });
        durable.invocation.summary =
            Some(InvocationSummaryHeader::from(&InvocationSummary::default()));
        persist_manifest(&paths, &mut durable, true).await.unwrap();
        assert_eq!(read_changes(root.path()).unwrap(), changes);
        drop(writer);

        assert_eq!(
            observer
                .enforce_retention(Duration::ZERO, u64::MAX)
                .await
                .unwrap(),
            1
        );
        assert!(matches!(
            observer.get_invocation(id).await,
            Err(StoreError::NotFound(missing)) if missing == id
        ));
    }

    #[test]
    fn multiprocess_shared_cache_helper() {
        let Ok(cache_root) = std::env::var("BAZEL_MCP_STORE_HELPER_ROOT") else {
            return;
        };
        let name = std::env::var("BAZEL_MCP_STORE_HELPER_NAME").unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let cache_root = PathBuf::from(cache_root);
                let store = Store::open(&cache_root).await.unwrap();
                let invocation = record(&cache_root.join(format!("worktree-{name}")));
                store.create_invocation(&invocation).await.unwrap();
                std::fs::write(cache_root.join(format!("ready-{name}")), b"ready").unwrap();
                let deadline = Instant::now() + Duration::from_secs(10);
                while !(cache_root.join("ready-a").is_file()
                    && cache_root.join("ready-b").is_file())
                {
                    assert!(
                        Instant::now() < deadline,
                        "timed out waiting for peer process"
                    );
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                loop {
                    let page = store
                        .list_invocations(None, None, None, PageRequest::new(None, 10))
                        .await
                        .unwrap();
                    if page.items.len() == 2 {
                        break;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "timed out waiting for peer invocation; observed {} record(s)",
                        page.items.len()
                    );
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            });
    }

    #[tokio::test]
    async fn two_os_processes_share_one_cache_root() {
        let root = TempDir::new().unwrap();
        let executable = std::env::current_exe().unwrap();
        let child = |name: &str| {
            let mut command = tokio::process::Command::new(&executable);
            command
                .arg("--exact")
                .arg("storage::tests::multiprocess_shared_cache_helper")
                .arg("--nocapture")
                .env("BAZEL_MCP_STORE_HELPER_ROOT", root.path())
                .env("BAZEL_MCP_STORE_HELPER_NAME", name)
                .kill_on_drop(true);
            command
        };
        let mut first = child("a");
        let mut second = child("b");
        let (first_output, second_output) = tokio::join!(first.output(), second.output());
        let first_output = first_output.unwrap();
        let second_output = second_output.unwrap();
        assert!(
            first_output.status.success(),
            "first process failed:\n{}",
            String::from_utf8_lossy(&first_output.stderr)
        );
        assert!(
            second_output.status.success(),
            "second process failed:\n{}",
            String::from_utf8_lossy(&second_output.stderr)
        );
    }

    #[tokio::test]
    async fn startup_rebuilds_workspace_and_deferred_ordering_indexes() {
        let root = TempDir::new().unwrap();
        let workspace_a = root.path().join("a");
        let workspace_b = root.path().join("b");
        let store = Store::open(root.path()).await.unwrap();
        let mut first = record(&workspace_a);
        first.request.requested_at_ms = 100;
        first.request.command = BazelCommand::Test;
        first.state = InvocationState::Failed;
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
            .list_invocations(Some(&workspace_a), None, None, PageRequest::new(None, 10))
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
        let failed_page = store
            .list_invocations(
                Some(&workspace_a),
                Some(InvocationState::Failed),
                None,
                PageRequest::default(),
            )
            .await
            .unwrap();
        assert_eq!(failed_page.items.len(), 1);
        assert_eq!(failed_page.items[0].request.id, first_id);
        let test_page = store
            .list_invocations(
                Some(&workspace_a),
                None,
                Some(&BazelCommand::Test),
                PageRequest::default(),
            )
            .await
            .unwrap();
        assert_eq!(test_page.items.len(), 1);
        assert_eq!(test_page.items[0].request.id, first_id);
        let deferred_page = store
            .list_deferred_results(DeferredRetrieval::InlineResult, 301, PageRequest::default())
            .await
            .unwrap();
        assert_eq!(deferred_page.items[0].deferred.invocation_id, third_id);
        drop(store);

        let reopened = Store::open(root.path()).await.unwrap();
        let rebuilt = reopened
            .list_invocations(Some(&workspace_a), None, None, PageRequest::default())
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
            .page_query_rows(id, Some("needle"), PageRequest::new(None, 1))
            .await
            .unwrap();
        assert_eq!(first.items[0].ordinal, 1);
        assert_eq!(
            (first.total_count, first.filtered_count),
            (Some(3), Some(2))
        );
        assert!(first.truncated);
        let second = store
            .page_query_rows(id, Some("needle"), PageRequest::new(first.next_cursor, 1))
            .await
            .unwrap();
        assert_eq!(second.items[0].ordinal, 2);
    }

    #[tokio::test]
    async fn query_scan_limits_resume_without_gaps_when_a_page_has_no_matches() {
        let root = TempDir::new().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let record = record(root.path());
        let id = record.request.id;
        let paths = store.create_invocation(&record).await.unwrap();
        tokio::fs::write(&paths.stdout, b"zero\none\nneedle\nthree\n")
            .await
            .unwrap();
        let first = store
            .page_query_rows(
                id,
                Some("needle"),
                PageRequest {
                    cursor: None,
                    item_limit: 1,
                    scan_limit: 2,
                },
            )
            .await
            .unwrap();
        assert!(first.items.is_empty());
        assert!(first.truncated);
        assert_eq!(first.total_count, None);
        assert_eq!(first.filtered_count, None);

        let second = store
            .page_query_rows(
                id,
                Some("needle"),
                PageRequest {
                    cursor: first.next_cursor,
                    item_limit: 1,
                    scan_limit: 2,
                },
            )
            .await
            .unwrap();
        assert_eq!(second.items.len(), 1);
        assert_eq!(second.items[0].ordinal, 2);
        assert_eq!(second.items[0].value, "needle");
    }

    #[tokio::test]
    async fn buffered_query_transform_precedes_ascii_insensitive_filtering() {
        let root = TempDir::new().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let record = record(root.path());
        let id = record.request.id;
        let paths = store.create_invocation(&record).await.unwrap();
        tokio::fs::write(&paths.stdout, b"first\nMiXeD needle\nthird\n")
            .await
            .unwrap();
        let transformed = Arc::new(AtomicUsize::new(0));
        let observed = transformed.clone();
        let page = store
            .page_query_rows_mapped_into(
                id,
                Some("mixed NEEDLE"),
                PageRequest::default(),
                move |value, output| {
                    observed.fetch_add(1, AtomicOrdering::Relaxed);
                    output.clear();
                    output.push_str(value);
                },
            )
            .await
            .unwrap();

        assert_eq!((page.total_count, page.filtered_count), (Some(3), Some(1)));
        assert_eq!(page.items[0].value, "MiXeD needle");
        assert_eq!(transformed.load(AtomicOrdering::Relaxed), 3);
    }

    #[tokio::test]
    async fn item_limits_preserve_filter_and_cursor_continuity() {
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
            let page = store
                .page_query_rows(id, None, PageRequest::new(cursor, 1))
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

        let requested = store
            .page_query_rows(id, None, PageRequest::new(None, 3))
            .await
            .unwrap();
        assert_eq!(requested.items.len(), 3);
        assert!(requested.truncated);

        let filtered = store
            .page_query_rows(id, Some("row_3"), PageRequest::new(None, 100))
            .await
            .unwrap();
        assert_eq!(filtered.filtered_count, Some(1));
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
        let page = store
            .page_query_rows_mapped(id, None, PageRequest::new(None, 3), move |value| {
                observed.fetch_add(1, AtomicOrdering::Relaxed);
                value.to_owned()
            })
            .await
            .unwrap();
        assert_eq!(
            (page.total_count, page.filtered_count),
            (Some(100), Some(100))
        );
        assert_eq!(page.items.len(), 3);
        assert_eq!(transformed.load(AtomicOrdering::Relaxed), 3);
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
        assert_eq!(store.count_query_rows(id).await.unwrap(), 3);
        let first = store
            .page_query_rows(id, None, PageRequest::new(None, 2))
            .await
            .unwrap();
        assert_eq!(
            (first.total_count, first.filtered_count),
            (Some(3), Some(3))
        );
        assert_eq!(first.items[0].value, "first");
        assert_eq!(first.items[1].value, "invalid-�");
        let last = store
            .page_query_rows(id, None, PageRequest::new(first.next_cursor, 2))
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
                    run: None,
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
        let header = reopened.get_invocation_header(id).await.unwrap();
        assert_eq!(header.summary.as_ref().unwrap().target_counts.requested, 1);
        let hydrated = reopened.get_hydrated_invocation(id).await.unwrap();
        assert_eq!(
            hydrated.details.targets[0].label,
            "//private:large-target-detail"
        );
        assert_eq!(
            hydrated.details.tests[0].label,
            "//private:large-test-detail"
        );
        assert_eq!(
            hydrated.details.coverage_files[0].path,
            "private/large-coverage-detail.rs"
        );
        assert_eq!(
            reopened
                .get_invocation(id)
                .await
                .unwrap()
                .summary
                .unwrap()
                .targets[0]
                .label,
            "//private:large-target-detail"
        );
        assert_eq!(
            reopened
                .page_tests(id, None, PageRequest::default())
                .await
                .unwrap()
                .items[0]
                .label,
            "//private:large-test-detail"
        );
        assert_eq!(
            reopened
                .page_coverage(id, None, PageRequest::default())
                .await
                .unwrap()
                .items[0]
                .path,
            "private/large-coverage-detail.rs"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn canonical_directories_and_files_remain_private() {
        use std::os::unix::fs::PermissionsExt;

        let container = TempDir::new().unwrap();
        let cache_root = container.path().join("cache");
        let store = Store::open(&cache_root).await.unwrap();
        let record = record(container.path());
        let paths = store.create_invocation(&record).await.unwrap();
        let shard = paths.directory.parent().unwrap();
        let day = shard.parent().unwrap();
        for directory in [
            cache_root.clone(),
            cache_root.join("invocations"),
            cache_root.join("trash"),
            cache_root.join("owners"),
            cache_root.join("mutations"),
            cache_root.join("changes"),
            day.to_owned(),
            shard.to_owned(),
            paths.directory.clone(),
        ] {
            assert_eq!(
                std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
                0o700,
                "{} was not private",
                directory.display()
            );
        }
        let (change_marker, change_lease) = store.inner.changes.publisher_paths(&cache_root).await;
        for file in [
            cache_root.join("MAINTENANCE"),
            change_lease,
            change_marker,
            owner_lock_path(&cache_root, record.request.id),
            mutation_lock_path(&cache_root, record.request.id),
            paths.manifest.clone(),
        ] {
            assert_eq!(
                std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
                0o600,
                "{} was not private",
                file.display()
            );
        }
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
    async fn pending_telemetry_can_be_flushed_before_shutdown() {
        let root = TempDir::new().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let record = record(root.path());
        let id = record.request.id;
        store.create_invocation(&record).await.unwrap();
        succeed(&store, id).await;
        let before = store.io_stats().manifest_commits;
        store
            .record_model_visible_result(id, 321, true)
            .await
            .unwrap();
        assert_eq!(store.io_stats().manifest_commits, before);
        assert_eq!(store.flush_pending_telemetry().await.unwrap(), 1);
        assert_eq!(store.io_stats().manifest_commits, before + 1);
        drop(store);

        let reopened = Store::open(root.path()).await.unwrap();
        let durable = reopened.get_invocation(id).await.unwrap();
        assert_eq!(durable.metrics.model_visible_bytes, 321);
        assert_eq!(durable.metrics.inspect_calls, 1);
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

    #[test]
    fn retention_age_cutoff_is_inclusive() {
        assert!(retention_age_elapsed(42, 42));
        assert!(retention_age_elapsed(41, 42));
        assert!(!retention_age_elapsed(43, 42));
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
                    run: None,
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
        let page = store
            .page_query_rows(id, None, PageRequest::default())
            .await
            .unwrap();
        assert_eq!((page.total_count, page.filtered_count), (Some(1), Some(1)));
        assert_eq!(page.items[0].value.len(), QUERY_LINE_LIMIT);
    }

    #[tokio::test]
    async fn query_reader_handles_empty_and_million_row_files() {
        let root = TempDir::new().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let empty = record(root.path());
        let empty_id = empty.request.id;
        store.create_invocation(&empty).await.unwrap();
        let page = store
            .page_query_rows(empty_id, None, PageRequest::default())
            .await
            .unwrap();
        assert!(page.items.is_empty());
        assert_eq!((page.total_count, page.filtered_count), (Some(0), Some(0)));

        let million = record(root.path());
        let million_id = million.request.id;
        let paths = store.create_invocation(&million).await.unwrap();
        let mut writer = std::io::BufWriter::new(std::fs::File::create(&paths.stdout).unwrap());
        for _ in 0..1_000_000 {
            writer.write_all(b"row\n").unwrap();
        }
        writer.flush().unwrap();
        let page = store
            .page_query_rows(million_id, None, PageRequest::new(None, 3))
            .await
            .unwrap();
        assert_eq!(page.items.len(), 3);
        assert_eq!((page.total_count, page.filtered_count), (None, None));
        assert!(page.truncated);
    }

    #[tokio::test]
    async fn failed_gc_unlink_leaves_recoverable_trash_without_reindexing() {
        let root = TempDir::new().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let record = record(root.path());
        let id = record.request.id;
        store.create_invocation(&record).await.unwrap();
        succeed(&store, id).await;
        store.inner.metrics.inject_gc_unlink_failure();

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
