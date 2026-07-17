//! In-process invocation admission, serialization, and cancellation state.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex, MutexGuard, Weak},
};

use bazel_mcp_types::{InvocationId, InvocationState};
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore, watch};
use tokio_util::sync::CancellationToken;

use crate::output_base_lock::OutputBaseWaitStatus;

/// Owns in-process admission, execution, workspace serialization, and
/// cancellation/progress registration for Bazel invocations.
#[derive(Clone)]
pub(crate) struct InvocationScheduler {
    global: Arc<Semaphore>,
    pending: Arc<Semaphore>,
    workspace_locks: Arc<Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>>,
    live: Arc<StdMutex<HashMap<InvocationId, LiveInvocation>>>,
    active_count: watch::Sender<usize>,
}

struct LiveInvocation {
    cancellation: CancellationToken,
    output_base_wait: Arc<OutputBaseWaitStatus>,
    lifecycle: watch::Sender<InvocationState>,
}

impl InvocationScheduler {
    pub(crate) fn new(global_concurrency: usize, maximum_pending: usize) -> Self {
        let (active_count, _) = watch::channel(0);
        Self {
            global: Arc::new(Semaphore::new(global_concurrency)),
            pending: Arc::new(Semaphore::new(maximum_pending)),
            workspace_locks: Arc::new(Mutex::new(HashMap::new())),
            live: Arc::new(StdMutex::new(HashMap::new())),
            active_count,
        }
    }

    fn live(&self) -> MutexGuard<'_, HashMap<InvocationId, LiveInvocation>> {
        self.live.lock().unwrap_or_else(|error| error.into_inner())
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

    pub(crate) fn register(
        &self,
        id: InvocationId,
        cancellation: CancellationToken,
        output_base_wait: Arc<OutputBaseWaitStatus>,
        initial_state: InvocationState,
    ) {
        let (lifecycle, _) = watch::channel(initial_state);
        let mut live = self.live();
        live.insert(
            id,
            LiveInvocation {
                cancellation,
                output_base_wait,
                lifecycle,
            },
        );
        self.active_count.send_replace(live.len());
    }

    pub(crate) fn remove(&self, id: InvocationId) {
        let mut live = self.live();
        if live.remove(&id).is_some() {
            self.active_count.send_replace(live.len());
        }
    }

    pub(crate) fn cancellation(&self, id: InvocationId) -> Option<CancellationToken> {
        self.live()
            .get(&id)
            .map(|invocation| invocation.cancellation.clone())
    }

    pub(crate) fn output_base_wait(&self, id: InvocationId) -> Option<Arc<OutputBaseWaitStatus>> {
        self.live()
            .get(&id)
            .map(|invocation| invocation.output_base_wait.clone())
    }

    pub(crate) fn lifecycle(&self, id: InvocationId) -> Option<watch::Receiver<InvocationState>> {
        self.live()
            .get(&id)
            .map(|invocation| invocation.lifecycle.subscribe())
    }

    pub(crate) fn lifecycle_state(&self, id: InvocationId) -> Option<InvocationState> {
        self.live()
            .get(&id)
            .map(|invocation| *invocation.lifecycle.borrow())
    }

    pub(crate) fn publish_state(&self, id: InvocationId, state: InvocationState) {
        if let Some(invocation) = self.live().get(&id) {
            invocation.lifecycle.send_replace(state);
        }
    }

    pub(crate) fn cancel_all(&self) -> usize {
        let cancellations: Vec<_> = self
            .live()
            .values()
            .map(|invocation| invocation.cancellation.clone())
            .collect();
        let count = cancellations.len();
        for cancellation in cancellations {
            cancellation.cancel();
        }
        count
    }

    pub(crate) fn active_count(&self) -> usize {
        self.live().len()
    }

    pub(crate) async fn wait_until_idle(&self) {
        let mut active_count = self.active_count.subscribe();
        while *active_count.borrow_and_update() > 0 {
            if active_count.changed().await.is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Arc, time::Duration};

    use bazel_mcp_types::{InvocationId, InvocationState};
    use tokio_util::sync::CancellationToken;

    use crate::output_base_lock::OutputBaseWaitStatus;

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

    #[tokio::test]
    async fn lifecycle_watch_broadcasts_state_and_idle_without_polling() {
        let scheduler = InvocationScheduler::new(1, 1);
        let id = InvocationId::new();
        scheduler.register(
            id,
            CancellationToken::new(),
            Arc::new(OutputBaseWaitStatus::default()),
            InvocationState::Queued,
        );
        let mut lifecycle = scheduler.lifecycle(id).unwrap();
        assert_eq!(*lifecycle.borrow_and_update(), InvocationState::Queued);

        scheduler.publish_state(id, InvocationState::Starting);
        lifecycle.changed().await.unwrap();
        assert_eq!(*lifecycle.borrow_and_update(), InvocationState::Starting);

        let mut waiting = tokio::spawn({
            let scheduler = scheduler.clone();
            async move { scheduler.wait_until_idle().await }
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut waiting)
                .await
                .is_err()
        );

        scheduler.publish_state(id, InvocationState::Succeeded);
        scheduler.remove(id);
        waiting.await.unwrap();
        lifecycle.changed().await.unwrap();
        assert_eq!(*lifecycle.borrow_and_update(), InvocationState::Succeeded);
    }
}
