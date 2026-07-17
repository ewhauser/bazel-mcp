//! Coalesced invocation telemetry accumulation and manifest flushing.

use std::{sync::Arc, time::Duration};

use bazel_mcp_types::InvocationId;

use crate::{
    Store, StoreError,
    coordination::{ProcessLock, mutation_lock_path},
    index::{IndexEntry, merge_telemetry},
    manifest::read as read_durable,
};

struct TelemetryAccumulator;

impl TelemetryAccumulator {
    fn record_model_visible(entry: &mut IndexEntry, bytes: u64, inspection: bool) -> bool {
        let metrics = &mut entry.record.metrics;
        metrics.model_visible_bytes = metrics.model_visible_bytes.saturating_add(bytes);
        if inspection {
            metrics.inspect_calls = metrics.inspect_calls.saturating_add(1);
        }
        Self::mark_dirty(entry)
    }

    fn record_progress(entry: &mut IndexEntry, count: u64) -> bool {
        let metrics = &mut entry.record.metrics;
        metrics.progress_notifications = metrics.progress_notifications.saturating_add(count);
        Self::mark_dirty(entry)
    }

    fn mark_dirty(entry: &mut IndexEntry) -> bool {
        entry.telemetry_generation = entry.telemetry_generation.saturating_add(1);
        if entry.telemetry_flush_scheduled {
            false
        } else {
            entry.telemetry_flush_scheduled = true;
            true
        }
    }
}

impl Store {
    pub async fn record_model_visible_result(
        &self,
        id: InvocationId,
        bytes: u64,
        inspection: bool,
    ) -> Result<(), StoreError> {
        self.refresh_index_if_stale().await?;
        let schedule = {
            let mut index = self.inner.index.write().await;
            let entry = index.entries.get_mut(&id).ok_or(StoreError::NotFound(id))?;
            TelemetryAccumulator::record_model_visible(entry, bytes, inspection)
        };
        if schedule {
            self.schedule_telemetry_flush(id);
        }
        Ok(())
    }

    pub async fn record_progress_notifications(
        &self,
        id: InvocationId,
        count: u64,
    ) -> Result<(), StoreError> {
        self.refresh_index_if_stale().await?;
        let schedule = {
            let mut index = self.inner.index.write().await;
            let entry = index.entries.get_mut(&id).ok_or(StoreError::NotFound(id))?;
            TelemetryAccumulator::record_progress(entry, count)
        };
        if schedule {
            self.schedule_telemetry_flush(id);
        }
        Ok(())
    }

    pub async fn flush_pending_telemetry(&self) -> Result<usize, StoreError> {
        let ids = {
            let index = self.inner.index.read().await;
            index
                .entries
                .iter()
                .filter_map(|(id, entry)| entry.telemetry_flush_scheduled.then_some(*id))
                .collect::<Vec<_>>()
        };
        for id in &ids {
            while !self.flush_telemetry_once(*id).await? {}
        }
        Ok(ids.len())
    }

    fn schedule_telemetry_flush(&self, id: InvocationId) {
        let inner = Arc::downgrade(&self.inner);
        let cache_root = self.cache_root.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(250)).await;
                let Some(inner) = inner.upgrade() else {
                    return;
                };
                let store = Store {
                    cache_root: cache_root.clone(),
                    inner,
                };
                match store.flush_telemetry_once(id).await {
                    Ok(true) => return,
                    Ok(false) => {}
                    Err(_) => {
                        if let Some(entry) = store.inner.index.write().await.entries.get_mut(&id) {
                            entry.telemetry_flush_scheduled = false;
                        }
                        return;
                    }
                }
            }
        });
    }

    async fn flush_telemetry_once(&self, id: InvocationId) -> Result<bool, StoreError> {
        self.refresh_index_if_stale().await?;
        let mutation_lock = self.mutation_lock(id).await;
        let _guard = mutation_lock.lock().await;
        let (metrics, generation) = {
            let index = self.inner.index.read().await;
            let Some(entry) = index.entries.get(&id) else {
                return Ok(true);
            };
            if !entry.telemetry_flush_scheduled {
                return Ok(true);
            }
            (entry.record.metrics.clone(), entry.telemetry_generation)
        };
        let _process_mutation =
            ProcessLock::acquire(mutation_lock_path(&self.cache_root, id)).await?;
        let paths = self.paths_for_id(id);
        let (mut durable, _) = read_durable(&paths.manifest).await?;
        merge_telemetry(&metrics, &mut durable.invocation.metrics);
        let retained_bytes = self.persist_durable(&paths, &mut durable, false).await?;
        self.publish_change().await?;
        let mut index = self.inner.index.write().await;
        let (previous, clean) = {
            let Some(entry) = index.entries.get_mut(&id) else {
                return Ok(true);
            };
            let previous = entry.retained_bytes;
            entry.retained_bytes = retained_bytes;
            let clean = entry.telemetry_generation == generation;
            if clean {
                entry.telemetry_flush_scheduled = false;
            }
            (previous, clean)
        };
        index.retained_bytes = index.retained_bytes.saturating_sub(previous);
        index.retained_bytes = index.retained_bytes.saturating_add(retained_bytes);
        Ok(clean)
    }
}
