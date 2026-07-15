use std::path::{Path, PathBuf};

use bazel_mcp_types::{Artifact, InvocationId, InvocationRecord};
use sha2::{Digest, Sha256};
use tokio::fs;

use crate::StoreError;

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
    pub fn new(cache_root: &Path, workspace: &Path, id: InvocationId) -> Self {
        let hash = hex_digest(workspace);
        let directory = cache_root
            .join("workspaces")
            .join(hash)
            .join("invocations")
            .join(id.to_string());
        Self {
            request: directory.join("request.json"),
            metadata: directory.join("metadata.json"),
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
        fs::create_dir(&self.directory).await?;
        if let Err(error) = set_private_directory(&self.directory).await {
            let _ = fs::remove_dir(&self.directory).await;
            return Err(error);
        }
        Ok(())
    }

    pub async fn write_request(&self, record: &InvocationRecord) -> Result<(), StoreError> {
        write_json_atomic(&self.request, &record.request).await?;
        write_json_atomic(&self.metadata, record).await
    }

    pub async fn write_metadata(&self, record: &InvocationRecord) -> Result<(), StoreError> {
        write_json_atomic(&self.metadata, record).await?;
        if let Some(summary) = &record.summary {
            write_json_atomic(&self.summary, summary).await?;
        }
        Ok(())
    }

    pub async fn read_metadata(&self) -> Result<InvocationRecord, StoreError> {
        Ok(serde_json::from_slice(&fs::read(&self.metadata).await?)?)
    }

    pub async fn write_artifacts(&self, artifacts: &[Artifact]) -> Result<(), StoreError> {
        write_json_atomic(&self.artifacts, artifacts).await
    }
}

fn hex_digest(path: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(path.to_string_lossy().as_bytes());
    format!("{:x}", hasher.finalize())
}

async fn write_json_atomic<T: serde::Serialize + ?Sized>(
    path: &Path,
    value: &T,
) -> Result<(), StoreError> {
    let bytes = serde_json::to_vec_pretty(value)?;
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, bytes).await?;
    set_private_file(&temporary).await?;
    fs::rename(temporary, path).await?;
    Ok(())
}

#[cfg(unix)]
async fn set_private_directory(path: &Path) -> Result<(), StoreError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).await?;
    Ok(())
}

#[cfg(not(unix))]
async fn set_private_directory(_path: &Path) -> Result<(), StoreError> {
    Ok(())
}

#[cfg(unix)]
async fn set_private_file(path: &Path) -> Result<(), StoreError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
    Ok(())
}

#[cfg(not(unix))]
async fn set_private_file(_path: &Path) -> Result<(), StoreError> {
    Ok(())
}
