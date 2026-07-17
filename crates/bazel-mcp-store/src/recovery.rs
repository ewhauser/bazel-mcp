//! Startup index reconstruction and interrupted-invocation recovery.

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use bazel_mcp_types::{
    InspectHint, InvocationId, InvocationRecord, InvocationState, InvocationSummary, Termination,
};

use crate::{
    InvocationPaths, StoreError, StoreStartupStats,
    coordination::{ProcessLock, mutation_lock_path, owner_lock_path},
    files::{remove_if_exists, write_json_atomic},
    index::{Index, IndexEntry, insert as insert_index_entry, replace as replace_index_entry},
    manifest::{decode as decode_durable, read as read_durable},
    manifest_repository::{evidence_payload_size_blocking, persist},
    record::{InvocationDetails, InvocationHeader},
};

pub(crate) struct RecoveryManager;

impl RecoveryManager {
    pub(crate) async fn clean_trash(cache_root: &Path) -> Result<(), StoreError> {
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

    pub(crate) async fn load_index(
        cache_root: &Path,
        cleanup_temporary: bool,
    ) -> Result<(Index, StoreStartupStats), StoreError> {
        let cache_root = cache_root.to_owned();
        tokio::task::spawn_blocking(move || load_index_blocking(&cache_root, cleanup_temporary))
            .await?
    }

    pub(crate) async fn recover_interrupted(
        cache_root: &Path,
        index: &mut Index,
    ) -> Result<usize, StoreError> {
        let recovered = Self::recover_ids(cache_root, nonterminal_ids(index)).await?;
        let recovered_count = recovered.len();
        for (id, entry) in recovered {
            replace_index_entry(index, id, entry);
        }
        Ok(recovered_count)
    }

    pub(crate) async fn recover_ids(
        cache_root: &Path,
        ids: Vec<InvocationId>,
    ) -> Result<Vec<(InvocationId, IndexEntry)>, StoreError> {
        let mut recovered = Vec::new();
        for id in ids {
            let Some(owner) = ProcessLock::try_acquire(owner_lock_path(cache_root, id)).await?
            else {
                continue;
            };
            let mutation = ProcessLock::acquire(mutation_lock_path(cache_root, id)).await?;
            let paths = InvocationPaths::new(cache_root, id);
            let (mut durable, _) = match read_durable(&paths.manifest).await {
                Ok(durable) => durable,
                Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                    continue;
                }
                Err(error) => return Err(error),
            };
            if durable.invocation.state.is_terminal() {
                let manifest_bytes = tokio::fs::metadata(&paths.manifest).await?.len();
                let retained_bytes = durable.payload_bytes.saturating_add(manifest_bytes);
                recovered.push((id, durable.index_entry(retained_bytes)));
                continue;
            }
            let mut full_record = durable.invocation.clone().into_record();
            full_record.state = InvocationState::Interrupted;
            full_record.finished_at_ms = Some(bazel_mcp_types::unix_timestamp_ms());
            full_record.termination = Some(Termination::Interrupted);
            full_record.summary = Some(InvocationSummary {
                success: false,
                headline: "Invocation was interrupted when the previous server stopped".into(),
                truncated: true,
                inspect_hint: Some(InspectHint::Log),
                ..InvocationSummary::default()
            });
            if let Some(deferred) = durable.deferred.as_mut() {
                deferred.extend_terminal_expiry(
                    full_record
                        .finished_at_ms
                        .unwrap_or_else(bazel_mcp_types::unix_timestamp_ms),
                );
            }
            write_details(&paths, &full_record).await?;
            durable.invocation = InvocationHeader::from_record(&full_record);
            let outcome = persist(&paths, &mut durable, true).await?;
            recovered.push((id, durable.index_entry(outcome.retained_bytes)));
            drop(mutation);
            drop(owner);
        }
        Ok(recovered)
    }
}

pub(crate) fn nonterminal_ids(index: &Index) -> Vec<InvocationId> {
    index
        .entries
        .iter()
        .filter_map(|(id, entry)| (!entry.record.state.is_terminal()).then_some(*id))
        .collect()
}

fn load_index_blocking(
    cache_root: &Path,
    cleanup_temporary: bool,
) -> Result<(Index, StoreStartupStats), StoreError> {
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
                let read_started = Instant::now();
                let bytes = std::fs::read(&expected.manifest);
                stats.manifest_read_us = stats
                    .manifest_read_us
                    .saturating_add(elapsed_micros(read_started.elapsed()));
                match bytes {
                    Ok(bytes) => {
                        index_manifest_bytes(&expected, id, &bytes, &mut index, &mut stats)?;
                        if cleanup_temporary {
                            cleanup_temporary_files(cache_root, id, &expected.directory)?;
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        if let Some(_cleanup) =
                            ProcessLock::try_acquire_blocking(&mutation_lock_path(cache_root, id))?
                        {
                            match std::fs::read(&expected.manifest) {
                                Ok(bytes) => index_manifest_bytes(
                                    &expected, id, &bytes, &mut index, &mut stats,
                                )?,
                                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                                    match std::fs::remove_dir_all(directory.path()) {
                                        Ok(()) => {}
                                        Err(error)
                                            if error.kind() == std::io::ErrorKind::NotFound => {}
                                        Err(error) => return Err(error.into()),
                                    }
                                }
                                Err(error) => return Err(error.into()),
                            }
                        }
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

fn index_manifest_bytes(
    paths: &InvocationPaths,
    id: InvocationId,
    bytes: &[u8],
    index: &mut Index,
    stats: &mut StoreStartupStats,
) -> Result<(), StoreError> {
    let manifest_bytes = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    let decode_started = Instant::now();
    let mut durable = decode_durable(&paths.manifest, bytes)?;
    stats.manifest_decode_us = stats
        .manifest_decode_us
        .saturating_add(elapsed_micros(decode_started.elapsed()));
    if durable.invocation.request.id != id {
        return Err(StoreError::CorruptRecord {
            path: paths.manifest.clone(),
            message: "record ID does not match directory".into(),
        });
    }
    if !durable.invocation.state.is_terminal() {
        durable.payload_bytes = evidence_payload_size_blocking(paths)?;
    }
    let retained_bytes = durable.payload_bytes.saturating_add(manifest_bytes);
    let index_started = Instant::now();
    insert_index_entry(index, id, durable.index_entry(retained_bytes));
    stats.index_build_us = stats
        .index_build_us
        .saturating_add(elapsed_micros(index_started.elapsed()));
    Ok(())
}

fn cleanup_temporary_files(
    cache_root: &Path,
    id: InvocationId,
    directory: &Path,
) -> Result<(), StoreError> {
    let temporary = temporary_files(directory)?;
    if !temporary.is_empty()
        && let Some(_cleanup) =
            ProcessLock::try_acquire_blocking(&mutation_lock_path(cache_root, id))?
    {
        for path in temporary {
            let _ = std::fs::remove_file(path);
        }
    }
    Ok(())
}

fn temporary_files(directory: &Path) -> Result<Vec<PathBuf>, StoreError> {
    const NAMES: [&str; 6] = [
        "manifest.tmp",
        "details.tmp",
        "artifacts.tmp",
        "failure-evidence.tmp",
        "failed-test-logs.tmp",
        "failed-test-evidence.tmp",
    ];
    let mut temporary = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| NAMES.contains(&name))
        {
            temporary.push(entry.path());
        }
    }
    Ok(temporary)
}

fn elapsed_micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn parse_id(value: &str) -> Option<InvocationId> {
    serde_json::from_str::<InvocationId>(&format!("\"{value}\"")).ok()
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
