use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex},
    time::{Duration, Instant},
};

use bazel_mcp_types::{InvocationId, unix_timestamp_ms};
use serde::{Deserialize, Serialize};
use tokio::{task, task::JoinHandle};
use tokio_util::sync::CancellationToken;

const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(50);
const NATIVE_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(100);
const NATIVE_WAIT_DETECTION_WINDOW: Duration = Duration::from_secs(2);
const NATIVE_LOCK_MARKER: &[u8] = b"Another command holds the output base lock:";
const NATIVE_WAIT_MARKER: &[u8] = b"Waiting for it to complete...";
const NATIVE_WAIT_TAIL_BYTES: usize = 32 * 1024;
const OWNER_BYTES_LIMIT: u64 = 4 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OutputBaseWaitSnapshot {
    pub(crate) active: bool,
    pub(crate) elapsed_ms: u64,
    pub(crate) owner: Option<String>,
}

#[derive(Default)]
pub(crate) struct OutputBaseWaitStatus {
    inner: StdMutex<OutputBaseWaitTiming>,
}

#[derive(Default)]
struct OutputBaseWaitTiming {
    active_since: Option<Instant>,
    accumulated: Duration,
    owner: Option<String>,
}

impl OutputBaseWaitStatus {
    pub(crate) fn begin(&self, owner: Option<String>) {
        let mut timing = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if timing.active_since.is_none() {
            timing.active_since = Some(Instant::now());
            timing.owner = owner;
        } else if timing.owner.is_none() {
            timing.owner = owner;
        }
    }

    pub(crate) fn end(&self) {
        let mut timing = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(started) = timing.active_since.take() {
            timing.accumulated = timing.accumulated.saturating_add(started.elapsed());
        }
        timing.owner = None;
    }

    pub(crate) fn snapshot(&self) -> OutputBaseWaitSnapshot {
        let timing = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let elapsed = timing.accumulated.saturating_add(
            timing
                .active_since
                .map_or(Duration::ZERO, |started| started.elapsed()),
        );
        OutputBaseWaitSnapshot {
            active: timing.active_since.is_some(),
            elapsed_ms: duration_millis(elapsed),
            owner: timing.owner.clone(),
        }
    }
}

pub(crate) enum OutputBaseLockAcquisition {
    Acquired(OutputBaseLockGuard),
    Cancelled,
}

pub(crate) struct OutputBaseLockGuard {
    file: File,
}

impl Drop for OutputBaseLockGuard {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct LockOwner {
    pid: u32,
    invocation_id: InvocationId,
    acquired_at_ms: i64,
}

enum LockAttempt {
    Acquired(OutputBaseLockGuard),
    Busy(Option<LockOwner>),
}

pub(crate) fn default_output_base_lock_root() -> PathBuf {
    #[cfg(unix)]
    {
        // Use a process-environment-independent root so MCP instances launched
        // by different hosts still share one advisory-lock namespace.
        // SAFETY: geteuid has no preconditions and does not mutate memory.
        #[expect(
            unsafe_code,
            reason = "libc::geteuid is the Unix effective-user-id primitive"
        )]
        let identity = unsafe { libc::geteuid() };
        PathBuf::from("/tmp").join(format!("bazel-mcp-output-base-locks-{identity}"))
    }
    #[cfg(not(unix))]
    {
        let user = std::env::var_os("USERNAME")
            .or_else(|| std::env::var_os("USER"))
            .unwrap_or_default();
        let identity = blake3::hash(user.to_string_lossy().as_bytes()).to_hex()[..16].to_owned();
        std::env::temp_dir().join(format!("bazel-mcp-output-base-locks-{identity}"))
    }
}

pub(crate) async fn acquire(
    root: &Path,
    key: &Path,
    invocation_id: InvocationId,
    cancellation: CancellationToken,
    wait_status: Arc<OutputBaseWaitStatus>,
) -> Result<OutputBaseLockAcquisition, io::Error> {
    tokio::fs::create_dir_all(root).await?;
    set_private_directory(root).await?;
    let digest = output_base_key_digest(key);
    let path = root.join(format!("{digest}.lock"));
    let owner = LockOwner {
        pid: std::process::id(),
        invocation_id,
        acquired_at_ms: unix_timestamp_ms(),
    };
    let mut waiting = false;

    loop {
        if cancellation.is_cancelled() {
            if waiting {
                wait_status.end();
            }
            return Ok(OutputBaseLockAcquisition::Cancelled);
        }
        let attempt_path = path.clone();
        let attempt_owner = owner.clone();
        let attempt = task::spawn_blocking(move || try_lock(&attempt_path, &attempt_owner))
            .await
            .map_err(io::Error::other)??;
        match attempt {
            LockAttempt::Acquired(guard) => {
                if waiting {
                    wait_status.end();
                }
                return Ok(OutputBaseLockAcquisition::Acquired(guard));
            }
            LockAttempt::Busy(current_owner) => {
                if !waiting {
                    let owner = current_owner.map(|owner| format!("bazel_mcp:pid={}", owner.pid));
                    wait_status.begin(owner);
                    waiting = true;
                }
                tokio::select! {
                    () = cancellation.cancelled() => {
                        wait_status.end();
                        return Ok(OutputBaseLockAcquisition::Cancelled);
                    }
                    () = tokio::time::sleep(LOCK_POLL_INTERVAL) => {}
                }
            }
        }
    }
}

fn output_base_key_digest(key: &Path) -> blake3::Hash {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        blake3::hash(key.as_os_str().as_bytes())
    }
    #[cfg(not(unix))]
    {
        blake3::hash(key.to_string_lossy().as_bytes())
    }
}

fn try_lock(path: &Path, owner: &LockOwner) -> io::Result<LockAttempt> {
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    set_private_file(&file)?;
    match file.try_lock() {
        Ok(()) => {
            file.set_len(0)?;
            file.seek(SeekFrom::Start(0))?;
            serde_json::to_writer(&mut file, owner).map_err(io::Error::other)?;
            file.write_all(b"\n")?;
            file.flush()?;
            Ok(LockAttempt::Acquired(OutputBaseLockGuard { file }))
        }
        Err(std::fs::TryLockError::WouldBlock) => {
            file.seek(SeekFrom::Start(0))?;
            let mut bytes = Vec::new();
            file.take(OWNER_BYTES_LIMIT).read_to_end(&mut bytes)?;
            let owner = serde_json::from_slice(&bytes).ok();
            Ok(LockAttempt::Busy(owner))
        }
        Err(std::fs::TryLockError::Error(error)) => Err(error),
    }
}

pub(crate) struct NativeOutputBaseWaitObserver {
    cancellation: CancellationToken,
    task: Option<JoinHandle<()>>,
    wait_status: Arc<OutputBaseWaitStatus>,
}

impl NativeOutputBaseWaitObserver {
    pub(crate) async fn start(
        stdout: PathBuf,
        stderr: PathBuf,
        bep: PathBuf,
        output_base: Option<PathBuf>,
        wait_status: Arc<OutputBaseWaitStatus>,
    ) -> Self {
        let initial_bazel_lock_pid = match output_base.as_deref() {
            Some(output_base) => active_bazel_lock_owner(output_base).await,
            None => None,
        };
        if let Some(pid) = initial_bazel_lock_pid {
            wait_status.begin(Some(format!("bazel_client:pid={pid}")));
        }
        let cancellation = CancellationToken::new();
        let task_cancellation = cancellation.clone();
        let task_status = wait_status.clone();
        let task = tokio::spawn(async move {
            observe_native_wait(
                stdout,
                stderr,
                bep,
                initial_bazel_lock_pid,
                task_status,
                task_cancellation,
            )
            .await;
        });
        Self {
            cancellation,
            task: Some(task),
            wait_status,
        }
    }

    pub(crate) async fn finish(mut self) {
        self.cancellation.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
        self.wait_status.end();
    }
}

impl Drop for NativeOutputBaseWaitObserver {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.wait_status.end();
    }
}

async fn observe_native_wait(
    stdout: PathBuf,
    stderr: PathBuf,
    bep: PathBuf,
    initial_bazel_lock_pid: Option<u32>,
    wait_status: Arc<OutputBaseWaitStatus>,
    cancellation: CancellationToken,
) {
    let detection_started = Instant::now();
    let mut waiting = initial_bazel_lock_pid.is_some();
    let mut waiting_marker_end = None;
    let mut baseline_stdout = 0;
    let mut baseline_bep = 0;
    let mut baseline_stderr_len = 0;

    loop {
        if let Some(initial_pid) = initial_bazel_lock_pid
            && !process_exists(initial_pid)
        {
            wait_status.end();
            return;
        }
        let stderr_tail = read_bounded_tail(&stderr, NATIVE_WAIT_TAIL_BYTES)
            .await
            .unwrap_or_default();
        if !waiting {
            if contains(&stderr_tail, NATIVE_LOCK_MARKER)
                || contains(&stderr_tail, NATIVE_WAIT_MARKER)
            {
                wait_status.begin(Some("bazel_client".to_owned()));
                waiting = true;
                baseline_stdout = file_size(&stdout).await;
                baseline_bep = file_size(&bep).await;
            } else if detection_started.elapsed() >= NATIVE_WAIT_DETECTION_WINDOW
                || file_size(&stdout).await > 0
                || file_size(&bep).await > 0
            {
                return;
            }
        }

        if waiting {
            if let Some(position) = find(&stderr_tail, NATIVE_WAIT_MARKER) {
                let marker_end = position.saturating_add(NATIVE_WAIT_MARKER.len());
                waiting_marker_end.get_or_insert(marker_end);
                baseline_stderr_len = baseline_stderr_len.max(stderr_tail.len());
                if stderr_tail[marker_end..]
                    .iter()
                    .any(|byte| !byte.is_ascii_whitespace())
                {
                    wait_status.end();
                    return;
                }
            }
            if waiting_marker_end.is_some()
                && (file_size(&stdout).await > baseline_stdout
                    || file_size(&bep).await > baseline_bep
                    || stderr_tail.len() > baseline_stderr_len)
            {
                wait_status.end();
                return;
            }
        }

        tokio::select! {
            () = cancellation.cancelled() => {
                if waiting {
                    wait_status.end();
                }
                return;
            }
            () = tokio::time::sleep(NATIVE_WAIT_POLL_INTERVAL) => {}
        }
    }
}

async fn active_bazel_lock_owner(output_base: &Path) -> Option<u32> {
    let pid = read_bazel_lock_owner(output_base).await?;
    process_exists(pid).then_some(pid)
}

async fn read_bazel_lock_owner(output_base: &Path) -> Option<u32> {
    let bytes = read_bounded_prefix(&output_base.join("lock"), OWNER_BYTES_LIMIT)
        .await
        .ok()?;
    parse_bazel_lock_pid(&bytes)
}

fn parse_bazel_lock_pid(bytes: &[u8]) -> Option<u32> {
    let text = std::str::from_utf8(bytes).ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix("pid="))?
        .trim()
        .parse()
        .ok()
}

#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 performs process-existence and permission checks only.
    #[expect(
        unsafe_code,
        reason = "libc::kill is the POSIX process-existence primitive"
    )]
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(not(unix))]
fn process_exists(_pid: u32) -> bool {
    // Avoid treating stale Bazel lock metadata as a live wait on platforms
    // where the runner cannot safely check process liveness.
    false
}

async fn read_bounded_prefix(path: &Path, maximum_bytes: u64) -> io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;

    let file = tokio::fs::File::open(path).await?;
    let mut data = Vec::with_capacity(usize::try_from(maximum_bytes).unwrap_or(4 * 1024));
    file.take(maximum_bytes).read_to_end(&mut data).await?;
    Ok(data)
}

async fn read_bounded_tail(path: &Path, maximum_bytes: usize) -> io::Result<Vec<u8>> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};

    let mut file = tokio::fs::File::open(path).await?;
    let length = file.metadata().await?.len();
    let tail_length = length.min(u64::try_from(maximum_bytes).unwrap_or(u64::MAX));
    file.seek(SeekFrom::Start(length.saturating_sub(tail_length)))
        .await?;
    let mut data = Vec::with_capacity(usize::try_from(tail_length).unwrap_or(maximum_bytes));
    file.read_to_end(&mut data).await?;
    Ok(data)
}

async fn file_size(path: &Path) -> u64 {
    tokio::fs::metadata(path)
        .await
        .map_or(0, |metadata| metadata.len())
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    find(haystack, needle).is_some()
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    (!needle.is_empty())
        .then(|| {
            haystack
                .windows(needle.len())
                .position(|window| window == needle)
        })
        .flatten()
}

async fn set_private_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).await?;
    }
    Ok(())
}

fn set_private_file(file: &File) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_status_accumulates_multiple_segments() {
        let status = OutputBaseWaitStatus::default();
        status.begin(Some("first".to_owned()));
        assert!(status.snapshot().active);
        status.end();
        status.begin(Some("second".to_owned()));
        let snapshot = status.snapshot();
        assert!(snapshot.active);
        assert_eq!(snapshot.owner.as_deref(), Some("second"));
        status.end();
        assert!(!status.snapshot().active);
    }

    #[test]
    fn parses_only_the_bounded_bazel_lock_pid_field() {
        assert_eq!(
            parse_bazel_lock_pid(b"pid=12345\nowner=client\ncwd=/private/workspace\n"),
            Some(12_345)
        );
        assert_eq!(parse_bazel_lock_pid(b"owner=client\n"), None);
        assert_eq!(parse_bazel_lock_pid(b"pid=not-a-pid\n"), None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn native_observer_tracks_a_live_explicit_output_base_owner() {
        let root = tempfile::tempdir().unwrap();
        let stdout = root.path().join("stdout.log");
        let stderr = root.path().join("stderr.log");
        let bep = root.path().join("events.bep");
        for path in [&stdout, &stderr, &bep] {
            tokio::fs::write(path, b"").await.unwrap();
        }
        let mut holder = tokio::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let holder_pid = holder.id().unwrap();
        tokio::fs::write(
            root.path().join("lock"),
            format!("pid={holder_pid}\nowner=client\ncwd=/redacted\n"),
        )
        .await
        .unwrap();
        let status = Arc::new(OutputBaseWaitStatus::default());
        let observer = NativeOutputBaseWaitObserver::start(
            stdout,
            stderr,
            bep,
            Some(root.path().to_owned()),
            status.clone(),
        )
        .await;

        let snapshot = status.snapshot();
        assert!(snapshot.active);
        let expected_owner = format!("bazel_client:pid={holder_pid}");
        assert_eq!(snapshot.owner.as_deref(), Some(expected_owner.as_str()));
        holder.kill().await.unwrap();
        let _ = holder.wait().await;
        for _ in 0..100 {
            if !status.snapshot().active {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!status.snapshot().active);
        observer.finish().await;
    }

    #[tokio::test]
    async fn file_lock_wait_is_cancellable_and_released_by_drop() {
        let root = tempfile::tempdir().unwrap();
        let key = root.path().join("shared-output-base");
        let first_status = Arc::new(OutputBaseWaitStatus::default());
        let first = acquire(
            root.path(),
            &key,
            InvocationId::new(),
            CancellationToken::new(),
            first_status,
        )
        .await
        .unwrap();
        let OutputBaseLockAcquisition::Acquired(first) = first else {
            panic!("first lock acquisition was cancelled");
        };

        let cancellation = CancellationToken::new();
        let second_status = Arc::new(OutputBaseWaitStatus::default());
        let second = tokio::spawn({
            let root = root.path().to_owned();
            let key = key.clone();
            let cancellation = cancellation.clone();
            let status = second_status.clone();
            async move {
                acquire(&root, &key, InvocationId::new(), cancellation, status)
                    .await
                    .unwrap()
            }
        });
        while !second_status.snapshot().active {
            tokio::task::yield_now().await;
        }
        cancellation.cancel();
        assert!(matches!(
            second.await.unwrap(),
            OutputBaseLockAcquisition::Cancelled
        ));

        drop(first);
        let third = acquire(
            root.path(),
            &key,
            InvocationId::new(),
            CancellationToken::new(),
            Arc::new(OutputBaseWaitStatus::default()),
        )
        .await
        .unwrap();
        assert!(matches!(third, OutputBaseLockAcquisition::Acquired(_)));
    }
}
