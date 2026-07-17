//! In-memory index ownership and replacement coordination.

use tokio::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::index::{Index, merge_pending_telemetry};

pub(crate) struct IndexCoordinator {
    state: RwLock<Index>,
}

impl IndexCoordinator {
    pub(crate) fn new(index: Index) -> Self {
        Self {
            state: RwLock::new(index),
        }
    }

    pub(crate) async fn read(&self) -> RwLockReadGuard<'_, Index> {
        self.state.read().await
    }

    pub(crate) async fn write(&self) -> RwLockWriteGuard<'_, Index> {
        self.state.write().await
    }

    /// Replace disk-derived state without losing telemetry that has not yet
    /// reached a manifest.
    pub(crate) async fn replace_from_disk(&self, mut refreshed: Index) {
        {
            let previous = self.state.read().await;
            merge_pending_telemetry(&previous, &mut refreshed);
        }
        *self.state.write().await = refreshed;
    }
}
