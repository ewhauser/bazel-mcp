use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Seek, Write},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::os::unix::{fs::FileTypeExt, fs::OpenOptionsExt};

use bazel_mcp_bep::{
    DEFAULT_MAX_FRAME_BYTES, DEFAULT_MAX_STREAM_BYTES, DEFAULT_MAX_STREAM_EVENTS,
    IncrementalStreamControl, IncrementalStreamDecoder, StreamOutcome,
};
use bazel_mcp_bes::BesCapture;
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
    Fifo,
    Bes,
    PostHocFallback,
}

pub(crate) struct BepReduction {
    pub(crate) accumulator: BepAccumulator,
    pub(crate) outcome: StreamOutcome,
    pub(crate) source: BepReductionSource,
    pub(crate) finalize_ms: u64,
}

pub(crate) enum LiveBepCapture {
    Tail(IncrementalBepCapture),
    #[cfg(unix)]
    Fifo(FifoBepCapture),
    Bes(BesBepCapture),
}

impl LiveBepCapture {
    pub(crate) async fn finish(
        self,
        bes_completion_grace: Duration,
    ) -> Result<BepReduction, RunnerError> {
        match self {
            Self::Tail(capture) => capture.finish().await,
            #[cfg(unix)]
            Self::Fifo(capture) => capture.finish().await,
            Self::Bes(capture) => capture.finish(bes_completion_grace).await,
        }
    }
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

/// Owns the transport-neutral capture pipeline fed by the loopback BES.
///
/// The BES service forwards ordered framed payloads over a bounded channel and
/// waits for each acknowledgement. This worker acknowledges a frame only after
/// the private evidence writer has accepted it and before reduction observes
/// it, keeping durable raw evidence authoritative.
pub(crate) struct BesBepCapture {
    control: Option<BesCapture>,
    task: Option<JoinHandle<Result<BepReduction, RunnerError>>>,
    evidence_path: PathBuf,
    extension_limits: Option<(usize, usize)>,
}

impl BesBepCapture {
    pub(crate) fn start(
        mut control: BesCapture,
        evidence_path: PathBuf,
        extension_limits: Option<(usize, usize)>,
    ) -> Result<Self, RunnerError> {
        let events = control.take_events()?;
        let task_path = evidence_path.clone();
        let task = task::spawn_blocking(move || read_bes_bep(events, task_path, extension_limits));
        Ok(Self {
            control: Some(control),
            task: Some(task),
            evidence_path,
            extension_limits,
        })
    }

    async fn finish(mut self, completion_grace: Duration) -> Result<BepReduction, RunnerError> {
        let started = Instant::now();
        let capture_result = self
            .control
            .take()
            .expect("BES capture control must exist")
            .finish(completion_grace)
            .await;
        if let Err(error) = &capture_result {
            tracing::warn!(%error, "BES capture did not complete cleanly; reducing retained prefix");
        }
        let result = match self.task.take().expect("BES capture task must exist").await {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => {
                tracing::warn!(%error, "BES pipeline failed; decoding retained prefix");
                post_hoc_reduction(self.evidence_path.clone(), self.extension_limits).await?
            }
            Err(error) => {
                tracing::warn!(%error, "BES pipeline task failed; decoding retained prefix");
                post_hoc_reduction(self.evidence_path.clone(), self.extension_limits).await?
            }
        };
        if let Ok(stats) = capture_result {
            tracing::debug!(
                events = stats.event_count,
                bytes = stats.bep_bytes,
                "completed BES capture"
            );
        }
        Ok(BepReduction {
            finalize_ms: duration_millis(started.elapsed()),
            ..result
        })
    }
}

impl Drop for BesBepCapture {
    fn drop(&mut self) {
        self.control.take();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[cfg(unix)]
pub(crate) struct PreparedFifoBepCapture {
    fifo_path: Option<PathBuf>,
    reader: Option<File>,
    evidence: Option<File>,
}

#[cfg(unix)]
impl PreparedFifoBepCapture {
    pub(crate) fn prepare(evidence_path: &Path) -> io::Result<Self> {
        use nix::{sys::stat::Mode, unistd::mkfifo};

        let fifo_path = evidence_path.with_extension("bep.fifo");
        mkfifo(&fifo_path, Mode::from_bits_truncate(0o600)).map_err(io::Error::from)?;
        let prepared = (|| {
            if !std::fs::symlink_metadata(&fifo_path)?.file_type().is_fifo() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{} is not a FIFO", fifo_path.display()),
                ));
            }
            let reader = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&fifo_path)?;
            let evidence = private_file(evidence_path)?;
            Ok((reader, evidence))
        })();
        match prepared {
            Ok((reader, evidence)) => Ok(Self {
                fifo_path: Some(fifo_path),
                reader: Some(reader),
                evidence: Some(evidence),
            }),
            Err(error) => {
                let _ = std::fs::remove_file(&fifo_path);
                Err(error)
            }
        }
    }

    pub(crate) fn path(&self) -> &Path {
        self.fifo_path
            .as_deref()
            .expect("prepared FIFO path must exist")
    }

    pub(crate) fn start(
        mut self,
        evidence_path: PathBuf,
        server_pid: u32,
        client_pid: u32,
        extension_limits: Option<(usize, usize)>,
    ) -> FifoBepCapture {
        let finishing = Arc::new(AtomicBool::new(false));
        let observed_bytes = Arc::new(AtomicU64::new(0));
        let task_finishing = finishing.clone();
        let task_observed_bytes = observed_bytes.clone();
        let reader = self.reader.take().expect("prepared FIFO reader must exist");
        let evidence = self
            .evidence
            .take()
            .expect("prepared FIFO evidence file must exist");
        let cleanup = FifoCleanup(
            self.fifo_path
                .take()
                .expect("prepared FIFO path must exist"),
        );
        let task = task::spawn_blocking(move || {
            read_fifo_bep(
                reader,
                evidence,
                extension_limits,
                &task_finishing,
                &task_observed_bytes,
                server_pid,
                client_pid,
            )
        });
        FifoBepCapture {
            finishing,
            task: Some(task),
            evidence_path,
            extension_limits,
            observed_bytes,
            server_pid,
            client_pid,
            cleanup: Some(cleanup),
        }
    }
}

#[cfg(unix)]
impl Drop for PreparedFifoBepCapture {
    fn drop(&mut self) {
        if let Some(path) = self.fifo_path.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(unix)]
struct FifoCleanup(PathBuf);

#[cfg(unix)]
impl Drop for FifoCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(unix)]
pub(crate) struct FifoBepCapture {
    finishing: Arc<AtomicBool>,
    task: Option<JoinHandle<Result<BepReduction, RunnerError>>>,
    evidence_path: PathBuf,
    extension_limits: Option<(usize, usize)>,
    observed_bytes: Arc<AtomicU64>,
    server_pid: u32,
    client_pid: u32,
    cleanup: Option<FifoCleanup>,
}

#[cfg(unix)]
impl FifoBepCapture {
    async fn finish(mut self) -> Result<BepReduction, RunnerError> {
        let started = Instant::now();
        self.finishing.store(true, Ordering::Release);
        let result = match self.task.take().expect("FIFO task must exist").await {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => {
                tracing::warn!(%error, "FIFO BEP capture failed; decoding retained prefix");
                post_hoc_reduction(self.evidence_path.clone(), self.extension_limits).await?
            }
            Err(error) => {
                tracing::warn!(%error, "FIFO BEP capture task failed; decoding retained prefix");
                post_hoc_reduction(self.evidence_path.clone(), self.extension_limits).await?
            }
        };
        tracing::trace!(
            server_pid = self.server_pid,
            client_pid = self.client_pid,
            observed_bytes = self.observed_bytes.load(Ordering::Acquire),
            "finished FIFO BEP capture"
        );
        self.cleanup.take();
        Ok(BepReduction {
            finalize_ms: duration_millis(started.elapsed()),
            ..result
        })
    }
}

#[cfg(unix)]
impl Drop for FifoBepCapture {
    fn drop(&mut self) {
        self.finishing.store(true, Ordering::Release);
        self.cleanup.take();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

struct ReductionSubscriber {
    accumulator: BepAccumulator,
    decoded_bytes: usize,
    event_count: usize,
}

impl ReductionSubscriber {
    fn new(extension_limits: Option<(usize, usize)>) -> Self {
        Self {
            accumulator: new_accumulator(extension_limits),
            decoded_bytes: 0,
            event_count: 0,
        }
    }

    fn observe(&mut self, event: bazel_mcp_bep::BorrowedBepEvent<'_>) {
        self.decoded_bytes = self.decoded_bytes.saturating_add(event.frame_bytes().len());
        self.event_count = self.event_count.saturating_add(1);
        self.accumulator.observe_borrowed(event);
    }

    fn outcome(&self) -> StreamOutcome {
        StreamOutcome {
            event_count: self.event_count,
            decoded_bytes: self.decoded_bytes,
            terminal_error: None,
        }
    }
}

enum DurableBepWriter {
    /// Tail and post-hoc sources read bytes only after Bazel has written them to
    /// the private evidence file. The gate still accounts and hashes those
    /// exact bytes before downstream subscribers observe decoded events.
    AlreadyPersisted { hasher: Hasher, bytes: u64 },
    /// FIFO and BES sources must append exact framed bytes before reduction or
    /// any future externally visible subscriber can observe them.
    Append {
        file: File,
        hasher: Hasher,
        bytes: u64,
    },
}

impl DurableBepWriter {
    fn already_persisted() -> Self {
        Self::AlreadyPersisted {
            hasher: Hasher::new(),
            bytes: 0,
        }
    }

    fn append(file: File) -> Self {
        Self::Append {
            file,
            hasher: Hasher::new(),
            bytes: 0,
        }
    }

    fn commit(&mut self, raw: &[u8]) -> io::Result<()> {
        match self {
            Self::AlreadyPersisted { hasher, bytes } => {
                hasher.update(raw);
                *bytes = bytes.saturating_add(u64::try_from(raw.len()).unwrap_or(u64::MAX));
            }
            Self::Append {
                file,
                hasher,
                bytes,
            } => {
                file.write_all(raw)?;
                hasher.update(raw);
                *bytes = bytes.saturating_add(u64::try_from(raw.len()).unwrap_or(u64::MAX));
            }
        }
        Ok(())
    }

    fn begin_retry(&mut self) -> io::Result<()> {
        if let Self::Append {
            file,
            hasher,
            bytes,
        } = self
        {
            file.flush()?;
            file.set_len(0)?;
            file.seek(io::SeekFrom::Start(0))?;
            *hasher = Hasher::new();
            *bytes = 0;
        }
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Self::Append { file, .. } = self {
            file.flush()?;
        }
        Ok(())
    }

    fn bytes(&self) -> u64 {
        match self {
            Self::AlreadyPersisted { bytes, .. } | Self::Append { bytes, .. } => *bytes,
        }
    }

    fn digest(&self) -> [u8; 32] {
        match self {
            Self::AlreadyPersisted { hasher, .. } | Self::Append { hasher, .. } => {
                *hasher.clone().finalize().as_bytes()
            }
        }
    }
}

struct CapturePipeline {
    decoder: IncrementalStreamDecoder,
    durable: DurableBepWriter,
    reduction: ReductionSubscriber,
    saved_attempt: Option<ReductionSubscriber>,
    expecting_retry: bool,
    pending_raw: Vec<u8>,
    extension_limits: Option<(usize, usize)>,
}

struct CapturePipelineOutput {
    reduction: BepReduction,
    durable_bytes: u64,
    durable_digest: [u8; 32],
}

impl CapturePipeline {
    fn new(durable: DurableBepWriter, extension_limits: Option<(usize, usize)>) -> Self {
        Self {
            decoder: IncrementalStreamDecoder::new(
                DEFAULT_MAX_FRAME_BYTES,
                DEFAULT_MAX_STREAM_BYTES,
                DEFAULT_MAX_STREAM_EVENTS,
            ),
            durable,
            reduction: ReductionSubscriber::new(extension_limits),
            saved_attempt: None,
            expecting_retry: false,
            pending_raw: Vec::new(),
            extension_limits,
        }
    }

    fn ingest(&mut self, input: &[u8]) -> Result<(), RunnerError> {
        use bazel_mcp_bep::view::build_event::Payload;

        if input.is_empty() {
            return Ok(());
        }
        if self.decoder.is_terminal() {
            self.durable.commit(input)?;
            return Ok(());
        }

        self.pending_raw.extend_from_slice(input);
        let durable = &mut self.durable;
        let reduction = &mut self.reduction;
        let saved_attempt = &mut self.saved_attempt;
        let expecting_retry = &mut self.expecting_retry;
        let pending_raw = &mut self.pending_raw;
        let extension_limits = self.extension_limits;
        let mut pipeline_error = None;
        self.decoder.push_framed_borrowed(input, |event, framed| {
            if pipeline_error.is_some() {
                return IncrementalStreamControl::Continue;
            }
            let is_started = matches!(event.view().payload.as_ref(), Some(Payload::Started(_)));
            let is_remote_cache_evicted = matches!(
                event.view().payload.as_ref(),
                Some(Payload::Finished(finished))
                    if finished.exit_code.as_option().is_some_and(|code| code.code == 39)
            );
            let Some(raw) = pending_raw.get(..framed.len()) else {
                pipeline_error = Some(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "BEP frame exceeded retained source bytes",
                ));
                return IncrementalStreamControl::Continue;
            };
            if raw != framed {
                pipeline_error = Some(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "decoded BEP frame did not match retained source bytes",
                ));
                return IncrementalStreamControl::Continue;
            }
            if *expecting_retry && is_started {
                if let Err(error) = durable.begin_retry() {
                    pipeline_error = Some(error);
                    return IncrementalStreamControl::Continue;
                }
                *saved_attempt = None;
                *expecting_retry = false;
            }
            // This is the security boundary: exact raw bytes are accepted by
            // the local durable writer before reduction observes the event.
            if let Err(error) = durable.commit(raw) {
                pipeline_error = Some(error);
                return IncrementalStreamControl::Continue;
            }
            reduction.observe(event);
            pending_raw.drain(..framed.len());
            if is_remote_cache_evicted {
                *saved_attempt = Some(std::mem::replace(
                    reduction,
                    ReductionSubscriber::new(extension_limits),
                ));
                *expecting_retry = true;
                IncrementalStreamControl::ResetAfterFrame
            } else {
                IncrementalStreamControl::Continue
            }
        });
        if let Some(error) = pipeline_error {
            return Err(error.into());
        }
        if self.decoder.is_terminal() && !self.pending_raw.is_empty() {
            self.durable.commit(&self.pending_raw)?;
            self.pending_raw.clear();
        }
        Ok(())
    }

    fn durable_bytes(&self) -> u64 {
        self.durable.bytes()
    }

    fn finish(mut self, source: BepReductionSource) -> Result<CapturePipelineOutput, RunnerError> {
        if !self.pending_raw.is_empty() {
            self.durable.commit(&self.pending_raw)?;
            self.pending_raw.clear();
        }
        self.durable.flush()?;
        let decoder_outcome = self.decoder.finish();
        let (reduction, outcome) = if self.expecting_retry && self.reduction.event_count == 0 {
            let saved = self.saved_attempt.unwrap_or(self.reduction);
            let outcome = saved.outcome();
            (saved, outcome)
        } else {
            (self.reduction, decoder_outcome)
        };
        Ok(CapturePipelineOutput {
            reduction: BepReduction {
                accumulator: reduction.accumulator,
                outcome,
                source,
                finalize_ms: 0,
            },
            durable_bytes: self.durable.bytes(),
            durable_digest: self.durable.digest(),
        })
    }
}

#[cfg(unix)]
fn read_fifo_bep(
    mut reader: File,
    evidence: File,
    extension_limits: Option<(usize, usize)>,
    finishing: &AtomicBool,
    observed_bytes: &AtomicU64,
    server_pid: u32,
    client_pid: u32,
) -> Result<BepReduction, RunnerError> {
    let verification_file = evidence.try_clone()?;
    let mut pipeline = CapturePipeline::new(DurableBepWriter::append(evidence), extension_limits);
    let mut buffer = [0_u8; 64 * 1024];
    let mut server_exit_observed = false;
    loop {
        match reader.read(&mut buffer) {
            Ok(read) if read > 0 => {
                pipeline.ingest(&buffer[..read])?;
                observed_bytes.store(pipeline.durable_bytes(), Ordering::Release);
            }
            Ok(0) => {
                let client_alive = is_process_alive(client_pid);
                if finishing.load(Ordering::Acquire) || !client_alive {
                    break;
                }
                if !server_exit_observed && !is_process_alive(server_pid) {
                    server_exit_observed = true;
                    tracing::debug!(
                        server_pid,
                        client_pid,
                        "Bazel server exited while invocation client remained alive; awaiting reconnect"
                    );
                }
                thread::sleep(BEP_TAIL_POLL_INTERVAL);
            }
            Ok(_) => unreachable!("positive FIFO reads are handled above"),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                let client_alive = is_process_alive(client_pid);
                if finishing.load(Ordering::Acquire) || !client_alive {
                    break;
                }
                if !server_exit_observed && !is_process_alive(server_pid) {
                    server_exit_observed = true;
                    tracing::debug!(
                        server_pid,
                        client_pid,
                        "Bazel server exited while invocation client remained alive; awaiting reconnect"
                    );
                }
                thread::sleep(BEP_TAIL_POLL_INTERVAL);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
    let output = pipeline.finish(BepReductionSource::Fifo)?;
    let (final_bytes, final_digest) = hash_open_file(&verification_file)?;
    if output.durable_bytes != final_bytes || output.durable_digest != final_digest {
        return Err(io::Error::other("FIFO BEP evidence did not match captured bytes").into());
    }
    observed_bytes.store(output.durable_bytes, Ordering::Release);
    Ok(output.reduction)
}

fn read_bes_bep(
    mut events: tokio::sync::mpsc::Receiver<bazel_mcp_bes::BesStreamEvent>,
    evidence_path: PathBuf,
    extension_limits: Option<(usize, usize)>,
) -> Result<BepReduction, RunnerError> {
    let evidence = private_file(&evidence_path)?;
    let mut pipeline = Some(CapturePipeline::new(
        DurableBepWriter::append(evidence),
        extension_limits,
    ));
    while let Some(event) = events.blocking_recv() {
        if event.is_finished() {
            let result = pipeline
                .take()
                .expect("BES pipeline must exist before finish")
                .finish(BepReductionSource::Bes);
            event.acknowledge(result.as_ref().map(|_| ()).map_err(ToString::to_string));
            return result.map(|output| output.reduction);
        }

        let pipeline = pipeline
            .as_mut()
            .expect("BES pipeline must exist while receiving frames");
        let mut result = Ok(());
        for framed in event
            .framed_events()
            .expect("non-terminal BES events must carry framed bytes")
        {
            if let Err(error) = pipeline.ingest(framed) {
                result = Err(error);
                break;
            }
        }
        event.acknowledge(result.as_ref().map(|_| ()).map_err(ToString::to_string));
        result?;
    }

    pipeline
        .take()
        .expect("BES pipeline must exist when event channel closes")
        .finish(BepReductionSource::Bes)
        .map(|output| output.reduction)
}

#[cfg(unix)]
fn is_process_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    // SAFETY: signal 0 performs a POSIX liveness/permission check and does not
    // deliver a signal to the target process.
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(unix)]
fn hash_open_file(file: &File) -> io::Result<(u64, [u8; 32])> {
    let mut reader = file.try_clone()?;
    reader.seek(io::SeekFrom::Start(0))?;
    let mut hasher = Hasher::new();
    let mut bytes = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        bytes = bytes.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
    }
    Ok((bytes, *hasher.finalize().as_bytes()))
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
    let mut pipeline =
        CapturePipeline::new(DurableBepWriter::already_persisted(), extension_limits);
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read > 0 {
            pipeline.ingest(&buffer[..read])?;
            observed_bytes.store(pipeline.durable_bytes(), Ordering::Release);
            continue;
        }
        if finishing.load(Ordering::Acquire) {
            break;
        }
        thread::sleep(BEP_TAIL_POLL_INTERVAL);
    }

    let output = pipeline.finish(BepReductionSource::Incremental)?;
    let (final_bytes, final_digest) = hash_file(&path)?;
    if output.durable_bytes == final_bytes && output.durable_digest == final_digest {
        return Ok(output.reduction);
    }

    tracing::debug!(
        tailed_bytes = output.durable_bytes,
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
        Ok(mut file) => {
            let mut pipeline =
                CapturePipeline::new(DurableBepWriter::already_persisted(), extension_limits);
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let read = file.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                pipeline.ingest(&buffer[..read])?;
            }
            let output = pipeline.finish(BepReductionSource::PostHocFallback)?;
            Ok((output.reduction.accumulator, output.reduction.outcome))
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
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).read(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    options.open(path)
}

#[cfg(test)]
mod tests {
    use bazel_mcp_reducer::Budget;
    use tempfile::tempdir;
    use tokio::io::AsyncWriteExt;

    use super::*;

    #[cfg(unix)]
    #[test]
    fn private_files_are_private_at_creation_and_can_be_reopened() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempdir().unwrap();
        let path = root.path().join("capture.log");
        let mut file = private_file(&path).unwrap();
        file.write_all(b"evidence").unwrap();
        drop(file);

        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(private_file(&path).unwrap());
        assert_eq!(std::fs::metadata(path).unwrap().len(), 0);
    }

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

    #[test]
    fn persisted_and_streamed_sources_share_one_ordered_capture_pipeline() {
        let root = tempdir().unwrap();
        let fixture =
            include_bytes!("../../bazel-mcp-reducer/tests/fixtures/bazel-9/test-outcomes.bep");

        let mut persisted = CapturePipeline::new(DurableBepWriter::already_persisted(), None);
        for chunk in fixture.chunks(113) {
            persisted.ingest(chunk).unwrap();
        }
        let persisted = persisted.finish(BepReductionSource::Incremental).unwrap();

        let streamed_path = root.path().join("streamed.bep");
        let mut streamed = CapturePipeline::new(
            DurableBepWriter::append(private_file(&streamed_path).unwrap()),
            None,
        );
        for chunk in fixture.chunks(97) {
            streamed.ingest(chunk).unwrap();
        }
        let streamed = streamed.finish(BepReductionSource::Bes).unwrap();

        assert_eq!(std::fs::read(streamed_path).unwrap(), fixture);
        assert_eq!(persisted.durable_bytes, streamed.durable_bytes);
        assert_eq!(persisted.durable_digest, streamed.durable_digest);
        assert_outcomes_match(&persisted.reduction.outcome, &streamed.reduction.outcome);
        assert_reductions_match(
            persisted.reduction.accumulator,
            streamed.reduction.accumulator,
        );
    }

    #[test]
    fn durability_failure_prevents_reduction_from_observing_the_frame() {
        let root = tempdir().unwrap();
        let path = root.path().join("read-only.bep");
        std::fs::write(&path, []).unwrap();
        let read_only = File::open(path).unwrap();
        let mut pipeline = CapturePipeline::new(DurableBepWriter::append(read_only), None);
        let frame = retry_attempt(0, "must-not-be-reduced");

        assert!(pipeline.ingest(&frame).is_err());
        assert_eq!(pipeline.reduction.event_count, 0);
        assert_eq!(pipeline.durable_bytes(), 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fifo_capture_reconnects_and_retains_only_the_successful_retry() {
        let root = tempdir().unwrap();
        let evidence_path = root.path().join("events.bep");
        let prepared = PreparedFifoBepCapture::prepare(&evidence_path).unwrap();
        let fifo_path = prepared.path().to_owned();
        let pid = std::process::id();
        let capture = prepared.start(evidence_path.clone(), pid, pid, None);
        let abandoned = retry_attempt(39, "abandoned");
        let retained = retry_attempt(0, "retained");
        let writer = std::thread::spawn(move || {
            for attempt in [abandoned, retained.clone()] {
                let mut pipe = OpenOptions::new().write(true).open(&fifo_path).unwrap();
                pipe.write_all(&attempt).unwrap();
            }
            retained
        });
        let retained = writer.join().unwrap();

        let reduction = capture.finish().await.unwrap();
        assert_eq!(reduction.source, BepReductionSource::Fifo);
        assert_eq!(reduction.outcome.event_count, 3);
        assert_eq!(std::fs::read(evidence_path).unwrap(), retained);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fifo_capture_preserves_evicted_attempt_when_no_retry_connects() {
        let root = tempdir().unwrap();
        let evidence_path = root.path().join("events.bep");
        let prepared = PreparedFifoBepCapture::prepare(&evidence_path).unwrap();
        let fifo_path = prepared.path().to_owned();
        let pid = std::process::id();
        let capture = prepared.start(evidence_path.clone(), pid, pid, None);
        let evicted = retry_attempt(39, "evicted");
        let expected = evicted.clone();
        std::thread::spawn(move || {
            let mut pipe = OpenOptions::new().write(true).open(fifo_path).unwrap();
            pipe.write_all(&evicted).unwrap();
        })
        .join()
        .unwrap();

        let reduction = capture.finish().await.unwrap();
        assert_eq!(reduction.outcome.event_count, 3);
        assert_eq!(std::fs::read(evidence_path).unwrap(), expected);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fifo_capture_preserves_a_truncated_raw_suffix() {
        let root = tempdir().unwrap();
        let evidence_path = root.path().join("events.bep");
        let prepared = PreparedFifoBepCapture::prepare(&evidence_path).unwrap();
        let fifo_path = prepared.path().to_owned();
        let pid = std::process::id();
        let capture = prepared.start(evidence_path.clone(), pid, pid, None);
        let mut truncated = retry_attempt(0, "truncated");
        truncated.pop();
        let expected = truncated.clone();
        std::thread::spawn(move || {
            let mut pipe = OpenOptions::new().write(true).open(fifo_path).unwrap();
            pipe.write_all(&truncated).unwrap();
        })
        .join()
        .unwrap();

        let reduction = capture.finish().await.unwrap();
        assert!(reduction.outcome.terminal_error.is_some());
        assert_eq!(std::fs::read(evidence_path).unwrap(), expected);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_fifo_capture_unlinks_a_never_connected_pipe() {
        let root = tempdir().unwrap();
        let evidence_path = root.path().join("events.bep");
        let prepared = PreparedFifoBepCapture::prepare(&evidence_path).unwrap();
        let fifo_path = prepared.path().to_owned();
        let pid = std::process::id();
        let capture = prepared.start(evidence_path, pid, pid, None);
        assert!(fifo_path.exists());

        drop(capture);
        assert!(!fifo_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn failed_fifo_preparation_removes_the_created_inode() {
        let root = tempdir().unwrap();
        let evidence_path = root.path().join("events.bep");
        std::fs::create_dir(&evidence_path).unwrap();
        let fifo_path = evidence_path.with_extension("bep.fifo");

        assert!(PreparedFifoBepCapture::prepare(&evidence_path).is_err());
        assert!(!fifo_path.exists());
    }

    fn retry_attempt(code: i32, marker: &str) -> Vec<u8> {
        use bazel_mcp_bep::{
            encode_frame,
            proto::{
                BuildEvent, BuildFinished, Progress, build_event::Payload, build_finished::ExitCode,
            },
        };
        use buffa::MessageField;

        let mut bytes = encode_frame(&BuildEvent {
            payload: Some(Payload::Started(Vec::new())),
            ..Default::default()
        });
        bytes.extend_from_slice(&encode_frame(&BuildEvent {
            payload: Some(Payload::Progress(Box::new(Progress {
                stdout: marker.to_owned(),
                stderr: String::new(),
            }))),
            ..Default::default()
        }));
        bytes.extend_from_slice(&encode_frame(&BuildEvent {
            last_message: true,
            payload: Some(Payload::Finished(Box::new(BuildFinished {
                overall_success: code == 0,
                exit_code: MessageField::some(ExitCode {
                    name: marker.to_owned(),
                    code,
                }),
                ..Default::default()
            }))),
            ..Default::default()
        }));
        bytes
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
