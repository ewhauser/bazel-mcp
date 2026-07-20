use std::{
    fs::{DirBuilder, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

use bazel_mcp_types::InvocationId;
use tokio::fs;

use crate::StoreError;

/// All durable files belonging to one invocation.
///
/// UUIDv7 embeds the creation timestamp, so its day bucket and final random
/// byte provide deterministic, bounded fan-out without a database lookup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvocationPaths {
    pub(crate) directory: PathBuf,
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

    pub(crate) async fn create(&self) -> Result<(), StoreError> {
        let parent = self
            .directory
            .parent()
            .ok_or_else(|| io::Error::other("invocation directory does not have a parent"))?
            .to_owned();
        let directory = self.directory.clone();
        tokio::task::spawn_blocking(move || {
            create_private_directory_all_blocking(&parent)?;
            create_private_directory_blocking(&directory)
        })
        .await??;
        Ok(())
    }
}

pub(crate) async fn write_json_atomic<T: serde::Serialize + ?Sized>(
    path: &Path,
    value: &T,
) -> Result<(), StoreError> {
    let bytes = serde_json::to_vec(value)?;
    write_bytes_atomic(path, bytes).await
}

pub(crate) async fn write_bytes_atomic(path: &Path, bytes: Vec<u8>) -> Result<(), StoreError> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || write_bytes_atomic_blocking(&path, &bytes)).await??;
    Ok(())
}

pub(crate) async fn remove_if_exists(path: &Path) -> Result<(), StoreError> {
    match fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub(crate) async fn create_private_directory_all(path: &Path) -> Result<(), StoreError> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || create_private_directory_all_blocking(&path)).await??;
    Ok(())
}

pub(crate) async fn create_private_directory(path: &Path) -> Result<(), StoreError> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || create_private_directory_blocking(&path)).await??;
    Ok(())
}

fn create_private_directory_all_blocking(path: &Path) -> io::Result<()> {
    let mut builder = private_directory_builder();
    builder.recursive(true).create(path)
}

fn create_private_directory_blocking(path: &Path) -> io::Result<()> {
    private_directory_builder().create(path)
}

#[cfg(unix)]
fn private_directory_builder() -> DirBuilder {
    let mut builder = DirBuilder::new();
    builder.mode(0o700);
    builder
}

#[cfg(not(unix))]
fn private_directory_builder() -> DirBuilder {
    DirBuilder::new()
}

fn write_bytes_atomic_blocking(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let temporary = path.with_extension("tmp");
    let result = (|| {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        drop(file);
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn atomic_writes_are_private_and_replace_the_committed_file() {
        use std::os::unix::fs::PermissionsExt;

        let root = TempDir::new().unwrap();
        let path = root.path().join("manifest.json");
        write_bytes_atomic(&path, b"first".to_vec()).await.unwrap();
        write_bytes_atomic(&path, b"second".to_vec()).await.unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!path.with_extension("tmp").exists());
    }
}
