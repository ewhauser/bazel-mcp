//! Private, atomic persistence for derived invocation evidence.

use std::{io, path::Path};

use tokio::io::AsyncWriteExt;

pub(crate) async fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<(), io::Error> {
    let temporary = path.with_extension("tmp");
    let mut file = tokio::fs::File::create(&temporary).await?;
    file.write_all(bytes).await?;
    file.flush().await?;
    drop(file);
    set_private_file(&temporary).await?;
    tokio::fs::rename(temporary, path).await
}

#[cfg(unix)]
pub(crate) async fn set_private_file(path: &Path) -> Result<(), io::Error> {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await
}

#[cfg(not(unix))]
async fn set_private_file(_path: &Path) -> Result<(), io::Error> {
    Ok(())
}
