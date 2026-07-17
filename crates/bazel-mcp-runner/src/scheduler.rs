//! In-process invocation admission, serialization, and cancellation state.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Weak},
};

use bazel_mcp_types::InvocationId;
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::output_base_lock::OutputBaseWaitStatus;

/// Owns in-process admission, execution, workspace serialization, and
/// cancellation/progress registration for Bazel invocations.
#[derive(Clone)]
pub(crate) struct InvocationScheduler {
    global: Arc<Semaphore>,
    pending: Arc<Semaphore>,
    workspace_locks: Arc<Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>>,
    output_base_waits: Arc<Mutex<HashMap<InvocationId, Arc<OutputBaseWaitStatus>>>>,
    live: Arc<Mutex<HashMap<InvocationId, CancellationToken>>>,
}

impl InvocationScheduler {
    pub(crate) fn new(global_concurrency: usize, maximum_pending: usize) -> Self {
        Self {
            global: Arc::new(Semaphore::new(global_concurrency)),
            pending: Arc::new(Semaphore::new(maximum_pending)),
            workspace_locks: Arc::new(Mutex::new(HashMap::new())),
            output_base_waits: Arc::new(Mutex::new(HashMap::new())),
            live: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub(crate) fn try_acquire_pending(&self) -> Option<OwnedSemaphorePermit> {
        self.pending.clone().try_acquire_owned().ok()
    }

    pub(crate) async fn acquire_execution(&self) -> Option<OwnedSemaphorePermit> {
        self.global.clone().acquire_owned().await.ok()
    }

    pub(crate) async fn workspace_lock(&self, key: &Path) -> Arc<Mutex<()>> {
        let mut locks = self.workspace_locks.lock().await;
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(key).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(Mutex::new(()));
        locks.insert(key.to_owned(), Arc::downgrade(&lock));
        lock
    }

    pub(crate) async fn register(
        &self,
        id: InvocationId,
        cancellation: CancellationToken,
        output_base_wait: Arc<OutputBaseWaitStatus>,
    ) {
        self.live.lock().await.insert(id, cancellation);
        self.output_base_waits
            .lock()
            .await
            .insert(id, output_base_wait);
    }

    pub(crate) async fn remove(&self, id: InvocationId) {
        self.live.lock().await.remove(&id);
        self.output_base_waits.lock().await.remove(&id);
    }

    pub(crate) async fn cancellation(&self, id: InvocationId) -> Option<CancellationToken> {
        self.live.lock().await.get(&id).cloned()
    }

    pub(crate) async fn output_base_wait(
        &self,
        id: InvocationId,
    ) -> Option<Arc<OutputBaseWaitStatus>> {
        self.output_base_waits.lock().await.get(&id).cloned()
    }

    pub(crate) async fn cancel_all(&self) -> usize {
        let cancellations: Vec<_> = self.live.lock().await.values().cloned().collect();
        let count = cancellations.len();
        for cancellation in cancellations {
            cancellation.cancel();
        }
        count
    }

    pub(crate) async fn active_count(&self) -> usize {
        self.live.lock().await.len()
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Arc};

    use super::InvocationScheduler;

    #[tokio::test]
    async fn workspace_lock_registry_discards_inactive_output_bases() {
        let scheduler = InvocationScheduler::new(1, 1);

        for index in 0..100 {
            let lock = scheduler
                .workspace_lock(Path::new(&format!("/tmp/output-base-{index}")))
                .await;
            drop(lock);
        }
        let retained = scheduler.workspace_lock(Path::new("/tmp/retained")).await;

        assert_eq!(scheduler.workspace_locks.lock().await.len(), 1);
        assert!(Arc::strong_count(&retained) >= 1);
    }
}
