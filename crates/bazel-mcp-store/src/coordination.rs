//! Cross-task and cross-process coordination for store mutations and changes.

use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    sync::{
        Arc, Weak,
        atomic::{AtomicU64, Ordering},
    },
};

use bazel_mcp_types::InvocationId;
use tokio::sync::{Mutex, MutexGuard};

use crate::StoreError;

const CHANGE_POLL_INTERVAL_US: u64 = 1_000;

pub(crate) struct LeaseManager {
    mutation_locks: Mutex<BTreeMap<InvocationId, Weak<Mutex<()>>>>,
    owner_leases: Mutex<BTreeMap<InvocationId, ProcessLock>>,
}

impl LeaseManager {
    pub(crate) fn new() -> Self {
        Self {
            mutation_locks: Mutex::new(BTreeMap::new()),
            owner_leases: Mutex::new(BTreeMap::new()),
        }
    }

    pub(crate) async fn mutation_lock(&self, id: InvocationId) -> Arc<Mutex<()>> {
        let mut locks = self.mutation_locks.lock().await;
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(&id).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(Mutex::new(()));
        locks.insert(id, Arc::downgrade(&lock));
        lock
    }

    pub(crate) async fn acquire_owner(
        &self,
        cache_root: &Path,
        id: InvocationId,
    ) -> Result<ProcessLock, StoreError> {
        match ProcessLock::try_acquire(owner_lock_path(cache_root, id)).await? {
            Some(owner) => Ok(owner),
            None => Err(StoreError::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("invocation {id} is already owned by another process"),
            ))),
        }
    }

    pub(crate) async fn retain_owner(&self, id: InvocationId, owner: ProcessLock) {
        self.owner_leases.lock().await.insert(id, owner);
    }

    pub(crate) async fn release_owner(&self, id: InvocationId) {
        self.owner_leases.lock().await.remove(&id);
    }
}

pub(crate) struct ProcessLock {
    _file: File,
}

impl ProcessLock {
    pub(crate) async fn acquire(path: PathBuf) -> Result<Self, StoreError> {
        let lock = tokio::task::spawn_blocking(move || {
            let file = open_lock_file(&path)?;
            file.lock()?;
            Ok::<_, std::io::Error>(Self { _file: file })
        })
        .await??;
        Ok(lock)
    }

    pub(crate) async fn try_acquire(path: PathBuf) -> Result<Option<Self>, StoreError> {
        let lock = tokio::task::spawn_blocking(move || {
            let file = open_lock_file(&path)?;
            match file.try_lock() {
                Ok(()) => Ok(Some(Self { _file: file })),
                Err(std::fs::TryLockError::WouldBlock) => Ok(None),
                Err(std::fs::TryLockError::Error(error)) => Err(error),
            }
        })
        .await??;
        Ok(lock)
    }

    pub(crate) fn try_acquire_blocking(path: &Path) -> Result<Option<Self>, StoreError> {
        let file = open_lock_file(path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
        }
    }
}

fn open_lock_file(path: &Path) -> Result<File, std::io::Error> {
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

pub(crate) fn owner_lock_path(cache_root: &Path, id: InvocationId) -> PathBuf {
    cache_root.join("owners").join(format!("{id}.lock"))
}

pub(crate) fn mutation_lock_path(cache_root: &Path, id: InvocationId) -> PathBuf {
    cache_root.join("mutations").join(format!("{id}.lock"))
}

pub(crate) struct ChangeCoordinator {
    state: Mutex<ChangeState>,
    next_check_us: AtomicU64,
}

impl ChangeCoordinator {
    pub(crate) async fn open(cache_root: &Path) -> Result<Self, StoreError> {
        let publisher = ChangePublisher::create(cache_root).await?;
        cleanup_stale_change_publishers(cache_root)?;
        let observed = read_changes(cache_root)?;
        Ok(Self {
            state: Mutex::new(ChangeState {
                publisher,
                observed,
            }),
            next_check_us: AtomicU64::new(0),
        })
    }

    pub(crate) async fn publish(&self) -> Result<(), StoreError> {
        let mut state = self.state.lock().await;
        let (publisher, change) = state.publisher.publish()?;
        state.observed.insert(publisher, change);
        Ok(())
    }

    pub(crate) fn claim_check(&self, now_us: u64) -> bool {
        let next = self.next_check_us.load(Ordering::Acquire);
        now_us >= next
            && self
                .next_check_us
                .compare_exchange(
                    next,
                    now_us.saturating_add(CHANGE_POLL_INTERVAL_US),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
    }

    pub(crate) async fn begin_refresh(
        &self,
        cache_root: &Path,
        force: bool,
    ) -> Result<Option<ChangeRefresh<'_>>, StoreError> {
        let guard = self.state.lock().await;
        let changes = read_changes(cache_root)?;
        if !force && changes == guard.observed {
            return Ok(None);
        }
        Ok(Some(ChangeRefresh { guard, changes }))
    }

    #[cfg(test)]
    pub(crate) async fn publisher_paths(&self, cache_root: &Path) -> (PathBuf, PathBuf) {
        let state = self.state.lock().await;
        (
            state.publisher.marker.clone(),
            cache_root
                .join("changes")
                .join(format!("{}.lock", state.publisher.id)),
        )
    }
}

pub(crate) struct ChangeRefresh<'a> {
    guard: MutexGuard<'a, ChangeState>,
    changes: BTreeMap<uuid::Uuid, uuid::Uuid>,
}

impl ChangeRefresh<'_> {
    pub(crate) fn commit(mut self) {
        self.guard.observed = self.changes;
    }
}

struct ChangeState {
    publisher: ChangePublisher,
    observed: BTreeMap<uuid::Uuid, uuid::Uuid>,
}

struct ChangePublisher {
    id: uuid::Uuid,
    marker: PathBuf,
    _lease: ProcessLock,
}

impl ChangePublisher {
    async fn create(cache_root: &Path) -> Result<Self, StoreError> {
        let id = uuid::Uuid::now_v7();
        let change = uuid::Uuid::now_v7();
        let directory = cache_root.join("changes");
        let lease = ProcessLock::acquire(directory.join(format!("{id}.lock"))).await?;
        let marker = directory.join(format!("{id}.{change}"));
        create_private_marker(&marker)?;
        Ok(Self {
            id,
            marker,
            _lease: lease,
        })
    }

    fn publish(&mut self) -> Result<(uuid::Uuid, uuid::Uuid), StoreError> {
        let change = uuid::Uuid::now_v7();
        let next = self.marker.with_file_name(format!("{}.{change}", self.id));
        std::fs::rename(&self.marker, &next)?;
        self.marker = next;
        Ok((self.id, change))
    }
}

fn create_private_marker(path: &Path) -> Result<(), StoreError> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    drop(options.open(path)?);
    Ok(())
}

fn parse_change_marker(name: &std::ffi::OsStr) -> Option<(uuid::Uuid, uuid::Uuid)> {
    let (publisher, change) = name.to_str()?.split_once('.')?;
    Some((publisher.parse().ok()?, change.parse().ok()?))
}

pub(crate) fn read_changes(
    cache_root: &Path,
) -> Result<BTreeMap<uuid::Uuid, uuid::Uuid>, StoreError> {
    let mut changes = BTreeMap::new();
    for entry in std::fs::read_dir(cache_root.join("changes"))? {
        let entry = entry?;
        if let Some((publisher, change)) = parse_change_marker(&entry.file_name()) {
            changes.insert(publisher, change);
        }
    }
    Ok(changes)
}

fn cleanup_stale_change_publishers(cache_root: &Path) -> Result<(), StoreError> {
    let directory = cache_root.join("changes");
    let mut stale = Vec::new();
    for entry in std::fs::read_dir(&directory)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(std::ffi::OsStr::to_str) != Some("lock") {
            continue;
        }
        let Some(publisher) = path
            .file_stem()
            .and_then(std::ffi::OsStr::to_str)
            .and_then(|value| value.parse::<uuid::Uuid>().ok())
        else {
            continue;
        };
        if let Some(lease) = ProcessLock::try_acquire_blocking(&path)? {
            drop(lease);
            stale.push((publisher, path));
        }
    }
    let stale_ids = stale
        .iter()
        .map(|(publisher, _)| *publisher)
        .collect::<std::collections::BTreeSet<_>>();
    for entry in std::fs::read_dir(&directory)? {
        let entry = entry?;
        if let Some((publisher, _)) = parse_change_marker(&entry.file_name())
            && stale_ids.contains(&publisher)
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    for (_, lock) in stale {
        let _ = std::fs::remove_file(lock);
    }
    Ok(())
}
