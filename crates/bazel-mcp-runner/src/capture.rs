use std::{
    fs::{File, OpenOptions},
    io::{self, Read},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use bazel_mcp_bep::{
    DEFAULT_MAX_FRAME_BYTES, DEFAULT_MAX_STREAM_BYTES, DEFAULT_MAX_STREAM_EVENTS,
    IncrementalStreamDecoder, StreamOutcome, visit_stream_partial_bounded,
};
use bazel_mcp_reducer::BepAccumulator;
use bazel_mcp_store::InvocationPaths;
use blake3::Hasher;
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncSeekExt},
    task::{self, JoinHandle},
};

use crate::RunnerError;

const BEP_TAIL_POLL_INTERVAL: Duration = Duration::from_millis(2);
const PARALLEL_MMAP_HASH_THRESHOLD: u64 = 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BepReductionSource {
    Incremental,
    PostHocFallback,
}

pub(crate) struct BepReduction {
    pub(crate) accumulator: BepAccumulator,
    pub(crate) outcome: StreamOutcome,
    pub(crate) source: BepReductionSource,
    pub(crate) finalize_ms: u64,
}

/// Owns the live tail task for one build-like Bazel invocation.
///
/// Dropping the handle signals and aborts the task, while [`Self::finish`]
/// drains the final bytes, verifies that the tailed byte sequence is the final
/// retained file, and falls back to a post-exit decode if Bazel rewrote or
/// truncated it. Decoding runs on Tokio's blocking pool rather than an async
/// runtime worker.
pub(crate) struct IncrementalBepCapture {
    finishing: Arc<AtomicBool>,
    task: Option<JoinHandle<Result<BepReduction, RunnerError>>>,
    path: PathBuf,
    extension_limits: Option<(usize, usize)>,
    observed_bytes: Arc<AtomicU64>,
}

impl IncrementalBepCapture {
    pub(crate) fn start(path: PathBuf, extension_limits: Option<(usize, usize)>) -> Self {
        let finishing = Arc::new(AtomicBool::new(false));
        let observed_bytes = Arc::new(AtomicU64::new(0));
        let task_finishing = finishing.clone();
        let task_observed_bytes = observed_bytes.clone();
        let task_path = path.clone();
        let task = task::spawn_blocking(move || {
            tail_bep(
                task_path,
                extension_limits,
                &task_finishing,
                &task_observed_bytes,
            )
        });
        Self {
            finishing,
            task: Some(task),
            path,
            extension_limits,
            observed_bytes,
        }
    }

    pub(crate) async fn finish(mut self) -> Result<BepReduction, RunnerError> {
        let started = Instant::now();
        self.finishing.store(true, Ordering::Release);
        let result = match self.task.take().expect("tail task must exist").await {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => {
                tracing::warn!(%error, "incremental BEP tail failed; decoding retained file");
                post_hoc_reduction(self.path.clone(), self.extension_limits).await?
            }
            Err(error) => {
                tracing::warn!(%error, "incremental BEP tail task failed; decoding retained file");
                post_hoc_reduction(self.path.clone(), self.extension_limits).await?
            }
        };
        tracing::trace!(
            observed_bytes = self.observed_bytes.load(Ordering::Acquire),
            "finished incremental BEP tail"
        );
        Ok(BepReduction {
            finalize_ms: duration_millis(started.elapsed()),
            ..result
        })
    }

    #[cfg(test)]
    async fn wait_until_observed(&self, expected: u64) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while self.observed_bytes.load(Ordering::Acquire) < expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("incremental tail did not observe expected bytes");
    }
}

impl Drop for IncrementalBepCapture {
    fn drop(&mut self) {
        self.finishing.store(true, Ordering::Release);
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

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
    extension_limits: Option<(usize, usize)>,
) -> Result<(BepAccumulator, StreamOutcome), RunnerError> {
    task::spawn_blocking(move || reduce_bep_file(&path, extension_limits)).await?
}

fn tail_bep(
    path: PathBuf,
    extension_limits: Option<(usize, usize)>,
    finishing: &AtomicBool,
    observed_bytes: &AtomicU64,
) -> Result<BepReduction, RunnerError> {
    let mut file = File::open(&path)?;
    let mut accumulator = new_accumulator(extension_limits);
    let mut decoder = IncrementalStreamDecoder::new(
        DEFAULT_MAX_FRAME_BYTES,
        DEFAULT_MAX_STREAM_BYTES,
        DEFAULT_MAX_STREAM_EVENTS,
    );
    let mut buffer = [0_u8; 64 * 1024];
    let mut hasher = Hasher::new();
    let mut tailed_bytes = 0_u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read > 0 {
            hasher.update(&buffer[..read]);
            tailed_bytes = tailed_bytes.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
            decoder.push(&buffer[..read], |event| accumulator.observe(event));
            observed_bytes.store(tailed_bytes, Ordering::Release);
            continue;
        }
        if finishing.load(Ordering::Acquire) {
            break;
        }
        thread::sleep(BEP_TAIL_POLL_INTERVAL);
    }

    let outcome = decoder.finish();
    let tailed_digest = *hasher.finalize().as_bytes();
    let (final_bytes, final_digest) = hash_file(&path)?;
    if tailed_bytes == final_bytes && tailed_digest == final_digest {
        return Ok(BepReduction {
            accumulator,
            outcome,
            source: BepReductionSource::Incremental,
            finalize_ms: 0,
        });
    }

    tracing::debug!(
        tailed_bytes,
        final_bytes,
        "BEP file changed while it was tailed; decoding retained file"
    );
    post_hoc_reduction_file(&path, extension_limits)
}

async fn post_hoc_reduction(
    path: PathBuf,
    extension_limits: Option<(usize, usize)>,
) -> Result<BepReduction, RunnerError> {
    let (accumulator, outcome) = reduce_bep(path, extension_limits).await?;
    Ok(BepReduction {
        accumulator,
        outcome,
        source: BepReductionSource::PostHocFallback,
        finalize_ms: 0,
    })
}

fn post_hoc_reduction_file(
    path: &Path,
    extension_limits: Option<(usize, usize)>,
) -> Result<BepReduction, RunnerError> {
    let (accumulator, outcome) = reduce_bep_file(path, extension_limits)?;
    Ok(BepReduction {
        accumulator,
        outcome,
        source: BepReductionSource::PostHocFallback,
        finalize_ms: 0,
    })
}

fn reduce_bep_file(
    path: &Path,
    extension_limits: Option<(usize, usize)>,
) -> Result<(BepAccumulator, StreamOutcome), RunnerError> {
    match File::open(path) {
        Ok(file) => {
            let mut accumulator = new_accumulator(extension_limits);
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
    }
}

fn new_accumulator(extension_limits: Option<(usize, usize)>) -> BepAccumulator {
    extension_limits.map_or_else(BepAccumulator::default, |limits| {
        BepAccumulator::with_extension_events(limits.0, limits.1)
    })
}

fn hash_file(path: &Path) -> io::Result<(u64, [u8; 32])> {
    let bytes = std::fs::metadata(path)?.len();
    let mut hasher = Hasher::new();
    if bytes >= PARALLEL_MMAP_HASH_THRESHOLD {
        hasher.update_mmap_rayon(path)?;
    } else {
        let mut file = File::open(path)?;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
    }
    Ok((bytes, *hasher.finalize().as_bytes()))
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
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
    use bazel_mcp_reducer::Budget;
    use tempfile::tempdir;
    use tokio::io::AsyncWriteExt;

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
        use tokio::io::AsyncSeekExt;

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

    #[tokio::test]
    async fn incremental_bep_capture_matches_post_hoc_reduction() {
        let root = tempdir().unwrap();
        let path = root.path().join("events.bep");
        std::fs::File::create(&path).unwrap();
        let fixture =
            include_bytes!("../../bazel-mcp-reducer/tests/fixtures/bazel-9/test-outcomes.bep");
        let capture = IncrementalBepCapture::start(path.clone(), None);
        let mut writer = fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .await
            .unwrap();
        for chunk in fixture.chunks(113) {
            writer.write_all(chunk).await.unwrap();
            writer.flush().await.unwrap();
            tokio::task::yield_now().await;
        }
        drop(writer);

        let incremental = capture.finish().await.unwrap();
        let (post_hoc, post_hoc_outcome) = reduce_bep(path, None).await.unwrap();
        assert_eq!(incremental.source, BepReductionSource::Incremental);
        assert_outcomes_match(&incremental.outcome, &post_hoc_outcome);
        assert_reductions_match(incremental.accumulator, post_hoc);
    }

    #[tokio::test]
    async fn incremental_bep_capture_falls_back_after_file_rewrite() {
        let root = tempdir().unwrap();
        let path = root.path().join("events.bep");
        let fixture =
            include_bytes!("../../bazel-mcp-reducer/tests/fixtures/bazel-9/test-outcomes.bep");
        fs::write(&path, fixture).await.unwrap();
        let capture = IncrementalBepCapture::start(path.clone(), None);
        capture.wait_until_observed(fixture.len() as u64).await;

        let retained = &fixture[..fixture.len() / 2];
        fs::write(&path, retained).await.unwrap();
        let incremental = capture.finish().await.unwrap();
        let (post_hoc, post_hoc_outcome) = reduce_bep(path, None).await.unwrap();
        assert_eq!(incremental.source, BepReductionSource::PostHocFallback);
        assert_outcomes_match(&incremental.outcome, &post_hoc_outcome);
        assert_reductions_match(incremental.accumulator, post_hoc);
    }

    fn assert_outcomes_match(actual: &StreamOutcome, expected: &StreamOutcome) {
        assert_eq!(actual.event_count, expected.event_count);
        assert_eq!(actual.decoded_bytes, expected.decoded_bytes);
        assert_eq!(
            actual.terminal_error.as_ref().map(ToString::to_string),
            expected.terminal_error.as_ref().map(ToString::to_string)
        );
    }

    fn assert_reductions_match(actual: BepAccumulator, expected: BepAccumulator) {
        let budget = Budget {
            max_bytes: usize::MAX,
            max_items: usize::MAX,
        };
        let actual = actual.finish(&[], &[], Some(0), 1, budget);
        let expected = expected.finish(&[], &[], Some(0), 1, budget);
        assert_eq!(actual.summary, expected.summary);
        assert_eq!(actual.artifacts, expected.artifacts);
        assert_eq!(actual.canonical_arguments, expected.canonical_arguments);
        assert_eq!(actual.reducer_events, expected.reducer_events);
        assert_eq!(
            actual.reducer_input_truncated,
            expected.reducer_input_truncated
        );
    }
}
