use std::path::{Path, PathBuf};

use bazel_mcp_types::InvocationId;
use tokio::fs;

use crate::StoreError;

/// All durable files belonging to one invocation.
///
/// UUIDv7 embeds the creation timestamp, so its day bucket and final random
/// byte provide deterministic, bounded fan-out without a database lookup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvocationPaths {
    pub directory: PathBuf,
    pub manifest: PathBuf,
    pub details: PathBuf,
    pub stdout: PathBuf,
    pub stderr: PathBuf,
    /// Redacted, normalized, encoding-neutral failure evidence used by the
    /// public `log` inspection view.
    pub evidence: PathBuf,
    pub bep: PathBuf,
    pub artifacts: PathBuf,
    /// Immutable raw snapshot of failed-test logs.
    pub test_logs_raw: PathBuf,
    /// Redacted line records used by the `test_log` inspection view.
    pub test_log_evidence: PathBuf,
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
            manifest: directory.join("manifest.json"),
            details: directory.join("details.json"),
            stdout: directory.join("stdout.log"),
            stderr: directory.join("stderr.log"),
            evidence: directory.join("failure-evidence.jsonl"),
            bep: directory.join("events.bep"),
            artifacts: directory.join("artifacts.json"),
            test_logs_raw: directory.join("failed-test-logs.raw"),
            test_log_evidence: directory.join("failed-test-evidence.jsonl"),
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
}

pub(crate) async fn write_json_atomic<T: serde::Serialize + ?Sized>(
    path: &Path,
    value: &T,
) -> Result<(), StoreError> {
    let bytes = serde_json::to_vec(value)?;
    write_bytes_atomic(path, &bytes).await
}

pub(crate) async fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, bytes).await?;
    set_private_file(&temporary).await?;
    fs::rename(temporary, path).await?;
    Ok(())
}

pub(crate) async fn remove_if_exists(path: &Path) -> Result<(), StoreError> {
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
