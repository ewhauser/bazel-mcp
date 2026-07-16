use std::path::{Path, PathBuf};

use bazel_mcp_types::{Artifact, InvocationId, InvocationRecord};
use tokio::fs;

use crate::StoreError;

/// All durable files belonging to one invocation.
///
/// UUIDv7 embeds the creation timestamp, so its day bucket and final random
/// byte provide deterministic, bounded fan-out without a database lookup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvocationPaths {
    pub directory: PathBuf,
    pub request: PathBuf,
    pub metadata: PathBuf,
    pub stdout: PathBuf,
    pub stderr: PathBuf,
    pub bep: PathBuf,
    pub summary: PathBuf,
    pub artifacts: PathBuf,
}

impl InvocationPaths {
    #[must_use]
    pub fn new(cache_root: &Path, id: InvocationId) -> Self {
        let uuid = id.as_uuid();
        let timestamp_ms = (uuid.as_u128() >> 80) as u64;
        let day = timestamp_ms / 86_400_000;
        // Sixteen shards keep per-directory fan-out bounded without making
        // startup pay to enumerate hundreds of mostly empty directories.
        let shard = uuid.as_bytes()[15] & 0x0f;
        let directory = cache_root
            .join("invocations")
            .join(format!("{day:08x}"))
            .join(format!("{shard:02x}"))
            .join(id.to_string());
        Self {
            request: directory.join("request.json"),
            metadata: directory.join("record.json"),
            stdout: directory.join("stdout.log"),
            stderr: directory.join("stderr.log"),
            bep: directory.join("events.bep"),
            summary: directory.join("summary.json"),
            artifacts: directory.join("artifacts.json"),
            directory,
        }
    }

    pub async fn create(&self) -> Result<(), StoreError> {
        let parent = self
            .directory
            .parent()
            .ok_or_else(|| std::io::Error::other("invocation directory does not have a parent"))?;
        fs::create_dir_all(parent).await?;
        set_private_directory(parent).await?;
        if let Some(day) = parent.parent() {
            set_private_directory(day).await?;
        }
        fs::create_dir(&self.directory).await?;
        if let Err(error) = set_private_directory(&self.directory).await {
            let _ = fs::remove_dir(&self.directory).await;
            return Err(error);
        }
        Ok(())
    }

    pub async fn write_request(&self, record: &InvocationRecord) -> Result<(), StoreError> {
        write_json_atomic(&self.request, &record.request).await
    }

    pub async fn write_summary(&self, record: &InvocationRecord) -> Result<(), StoreError> {
        if let Some(summary) = &record.summary {
            write_json_atomic(&self.summary, summary).await?;
        } else {
            remove_if_exists(&self.summary).await?;
        }
        Ok(())
    }

    pub async fn write_artifacts(&self, artifacts: &[Artifact]) -> Result<(), StoreError> {
        write_json_atomic(&self.artifacts, artifacts).await
    }
}

pub(crate) async fn write_json_atomic<T: serde::Serialize + ?Sized>(
    path: &Path,
    value: &T,
) -> Result<(), StoreError> {
    let bytes = serde_json::to_vec(value)?;
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, bytes).await?;
    set_private_file(&temporary).await?;
    fs::rename(temporary, path).await?;
    Ok(())
}

async fn remove_if_exists(path: &Path) -> Result<(), StoreError> {
    match fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
pub(crate) async fn set_private_directory(path: &Path) -> Result<(), StoreError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).await?;
    Ok(())
}

#[cfg(not(unix))]
pub(crate) async fn set_private_directory(_path: &Path) -> Result<(), StoreError> {
    Ok(())
}

#[cfg(unix)]
pub(crate) async fn set_private_file(path: &Path) -> Result<(), StoreError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
    Ok(())
}

#[cfg(not(unix))]
pub(crate) async fn set_private_file(_path: &Path) -> Result<(), StoreError> {
    Ok(())
}
