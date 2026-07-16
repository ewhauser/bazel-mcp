use std::{
    fs::OpenOptions,
    io,
    path::{Path, PathBuf},
    process::Stdio,
};

use bazel_mcp_bep::{
    DEFAULT_MAX_FRAME_BYTES, DEFAULT_MAX_STREAM_BYTES, DEFAULT_MAX_STREAM_EVENTS, StreamOutcome,
    visit_stream_partial_bounded,
};
use bazel_mcp_reducer::BepAccumulator;
use bazel_mcp_store::InvocationPaths;
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncSeekExt},
    task,
};

use crate::RunnerError;

pub(crate) async fn open_stdio(paths: &InvocationPaths) -> Result<(Stdio, Stdio), RunnerError> {
    let stdout_path = paths.stdout.clone();
    let stderr_path = paths.stderr.clone();
    let bep_path = paths.bep.clone();
    Ok(task::spawn_blocking(move || {
        let stdout = private_file(&stdout_path)?;
        let stderr = private_file(&stderr_path)?;
        drop(private_file(&bep_path)?);
        Ok::<_, io::Error>((Stdio::from(stdout), Stdio::from(stderr)))
    })
    .await??)
}

pub(crate) async fn read_bounded_tail(
    path: &Path,
    max_bytes: usize,
) -> Result<Vec<u8>, RunnerError> {
    let mut file = match fs::File::open(path).await {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let length = file.metadata().await?.len();
    let tail_length = length.min(u64::try_from(max_bytes).unwrap_or(u64::MAX));
    file.seek(io::SeekFrom::Start(length - tail_length)).await?;
    let mut data = vec![0_u8; usize::try_from(tail_length).unwrap_or(max_bytes)];
    file.read_exact(&mut data).await?;
    Ok(data)
}

pub(crate) async fn reduce_bep(
    path: PathBuf,
) -> Result<(BepAccumulator, StreamOutcome), RunnerError> {
    task::spawn_blocking(move || match std::fs::File::open(path) {
        Ok(file) => {
            let mut accumulator = BepAccumulator::default();
            let outcome = visit_stream_partial_bounded(
                file,
                DEFAULT_MAX_FRAME_BYTES,
                DEFAULT_MAX_STREAM_BYTES,
                DEFAULT_MAX_STREAM_EVENTS,
                |event| accumulator.observe(event),
            );
            Ok((accumulator, outcome))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok((
            BepAccumulator::default(),
            StreamOutcome {
                event_count: 0,
                decoded_bytes: 0,
                terminal_error: None,
            },
        )),
        Err(error) => Err(RunnerError::Io(error)),
    })
    .await?
}

pub(crate) async fn file_size(path: &Path) -> u64 {
    fs::metadata(path)
        .await
        .map_or(0, |metadata| metadata.len())
}

fn private_file(path: &Path) -> io::Result<std::fs::File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[tokio::test]
    async fn bounded_tail_reads_only_the_requested_suffix() {
        let root = tempdir().unwrap();
        let path = root.path().join("large.log");
        let mut contents = vec![b'a'; 1024 * 1024];
        contents.extend_from_slice(b"useful-tail");
        fs::write(&path, contents).await.unwrap();

        assert_eq!(read_bounded_tail(&path, 11).await.unwrap(), b"useful-tail");
        assert!(read_bounded_tail(&path, 0).await.unwrap().is_empty());
        assert!(
            read_bounded_tail(&root.path().join("missing"), 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn bounded_tail_handles_sparse_multi_gigabyte_logs_without_scaling_memory() {
        use tokio::io::{AsyncSeekExt, AsyncWriteExt};

        let root = tempdir().unwrap();
        let path = root.path().join("multi-gigabyte.log");
        let mut file = fs::File::create(&path).await.unwrap();
        file.seek(io::SeekFrom::Start(4 * 1024 * 1024 * 1024))
            .await
            .unwrap();
        file.write_all(b"useful-tail").await.unwrap();
        file.flush().await.unwrap();

        assert_eq!(read_bounded_tail(&path, 11).await.unwrap(), b"useful-tail");
    }
}
