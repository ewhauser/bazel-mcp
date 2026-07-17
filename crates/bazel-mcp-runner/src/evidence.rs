//! Private, atomic persistence for derived invocation evidence.

use std::{io, path::Path};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

pub(crate) async fn write_private_atomic(path: &Path, bytes: Vec<u8>) -> Result<(), io::Error> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        use std::io::Write as _;

        let temporary = path.with_extension("tmp");
        let result = (|| {
            let mut options = std::fs::OpenOptions::new();
            options.create_new(true).write(true);
            #[cfg(unix)]
            options.mode(0o600);
            let mut file = options.open(&temporary)?;
            file.write_all(&bytes)?;
            file.flush()?;
            drop(file);
            std::fs::rename(&temporary, &path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(temporary);
        }
        result
    })
    .await
    .map_err(io::Error::other)?
}

pub(crate) async fn create_private_file(path: &Path) -> Result<tokio::fs::File, io::Error> {
    let mut options = tokio::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    options.open(path).await
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn private_atomic_writes_publish_private_complete_files() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempdir().unwrap();
        let path = root.path().join("failure-evidence.jsonl");
        write_private_atomic(&path, b"first".to_vec())
            .await
            .unwrap();
        write_private_atomic(&path, b"second".to_vec())
            .await
            .unwrap();

        assert_eq!(tokio::fs::read(&path).await.unwrap(), b"second");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!path.with_extension("tmp").exists());
    }
}
