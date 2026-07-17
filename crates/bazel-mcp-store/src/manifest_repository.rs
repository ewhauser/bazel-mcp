//! Filesystem manifest repository and retained-byte accounting.

use std::path::PathBuf;

use bazel_mcp_types::InvocationId;

use crate::{InvocationPaths, StoreError, files::write_bytes_atomic, manifest::DurableRecord};

pub(crate) struct ManifestRepository {
    cache_root: PathBuf,
}

impl ManifestRepository {
    pub(crate) fn new(cache_root: PathBuf) -> Self {
        Self { cache_root }
    }

    pub(crate) fn paths_for_id(&self, id: InvocationId) -> InvocationPaths {
        InvocationPaths::new(&self.cache_root, id)
    }

    pub(crate) async fn persist(
        &self,
        paths: &InvocationPaths,
        durable: &mut DurableRecord,
        recount_payload: bool,
    ) -> Result<PersistOutcome, StoreError> {
        persist(paths, durable, recount_payload).await
    }
}

pub(crate) struct PersistOutcome {
    pub(crate) retained_bytes: u64,
    pub(crate) manifest_bytes: u64,
}

pub(crate) async fn persist(
    paths: &InvocationPaths,
    durable: &mut DurableRecord,
    recount_payload: bool,
) -> Result<PersistOutcome, StoreError> {
    if recount_payload {
        durable.payload_bytes = evidence_payload_size(paths).await?;
    }
    let bytes = serde_json::to_vec(durable)?;
    let manifest_bytes = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    write_bytes_atomic(&paths.manifest, bytes).await?;
    Ok(PersistOutcome {
        retained_bytes: durable.payload_bytes.saturating_add(manifest_bytes),
        manifest_bytes,
    })
}

async fn evidence_payload_size(paths: &InvocationPaths) -> Result<u64, StoreError> {
    evidence_size_for(paths, false).await
}

pub(crate) async fn evidence_size(paths: &InvocationPaths) -> Result<u64, StoreError> {
    evidence_size_for(paths, true).await
}

async fn evidence_size_for(
    paths: &InvocationPaths,
    include_manifest: bool,
) -> Result<u64, StoreError> {
    let mut size = 0_u64;
    let files = [
        &paths.manifest,
        &paths.details,
        &paths.stdout,
        &paths.stderr,
        &paths.evidence,
        &paths.bep,
        &paths.artifacts,
        &paths.test_logs_raw,
        &paths.test_log_evidence,
    ];
    for path in files.into_iter().skip(usize::from(!include_manifest)) {
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

pub(crate) fn evidence_payload_size_blocking(paths: &InvocationPaths) -> Result<u64, StoreError> {
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
