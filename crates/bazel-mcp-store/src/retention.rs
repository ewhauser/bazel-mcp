//! Age- and quota-based retention for terminal invocation evidence.

use std::{
    collections::BTreeSet,
    time::{Duration, Instant},
};

use bazel_mcp_types::InvocationId;

use crate::{
    coordination::{ProcessLock, mutation_lock_path, owner_lock_path},
    index::{remove as remove_index_entry, replace as replace_index_entry},
    manifest::read as read_durable,
    manifest_repository::evidence_size,
    storage::{
        ReclaimOutcome, Store, StoreError, elapsed_us, finish_staged_evidence, rename_to_trash,
        retention_age_elapsed, stage_raw_evidence,
    },
};

const GC_LOW_WATERMARK_PERCENT: u64 = 80;
const GC_NOTIFICATION_BATCH_SIZE: usize = 64;

impl Store {
    pub async fn enforce_retention(
        &self,
        maximum_age: Duration,
        maximum_bytes: u64,
    ) -> Result<usize, StoreError> {
        let Some(_maintenance) =
            ProcessLock::try_acquire(self.cache_root.join("MAINTENANCE")).await?
        else {
            return Ok(0);
        };
        // A forced scan also repairs the narrow crash window between an
        // atomic manifest commit and its change notification.
        self.refresh_index(true).await?;
        self.recover_orphaned_invocations().await?;
        let now_ms = bazel_mcp_types::unix_timestamp_ms();
        self.delete_expired_deferred_results(now_ms).await?;
        let cutoff =
            now_ms.saturating_sub(i64::try_from(maximum_age.as_millis()).unwrap_or(i64::MAX));
        // Running evidence can grow without metadata commits. Refresh only the
        // bounded live set; terminal bytes remain commit-accounted, so normal
        // GC never walks the cache tree.
        let live_ids: Vec<_> = self
            .inner
            .index
            .read()
            .await
            .entries
            .iter()
            .filter_map(|(id, entry)| (!entry.record.state.is_terminal()).then_some(*id))
            .collect();
        for id in live_ids {
            let current = evidence_size(&self.paths_for_id(id)).await?;
            let mut index = self.inner.index.write().await;
            let previous = index.entries.get_mut(&id).and_then(|entry| {
                if entry.record.state.is_terminal() {
                    return None;
                }
                let previous = entry.retained_bytes;
                entry.retained_bytes = current;
                Some(previous)
            });
            if let Some(previous) = previous {
                index.retained_bytes = index.retained_bytes.saturating_sub(previous);
                index.retained_bytes = index.retained_bytes.saturating_add(current);
            }
        }
        let candidates: Vec<_> = {
            let index = self.inner.index.read().await;
            index
                .terminal_by_finished
                .iter()
                .map(|(finished, id)| (*id, *finished))
                .collect()
        };

        let low_watermark = maximum_bytes
            .saturating_mul(GC_LOW_WATERMARK_PERCENT)
            .checked_div(100)
            .unwrap_or(0);
        let mut reclaimed = 0;
        let mut pending_notifications = 0;
        let mut pending_lock_cleanup = Vec::new();
        let mut processed = BTreeSet::new();
        for (id, finished) in &candidates {
            if retention_age_elapsed(*finished, cutoff) {
                self.reclaim_retention_candidate(
                    *id,
                    &mut reclaimed,
                    &mut pending_notifications,
                    &mut pending_lock_cleanup,
                )
                .await?;
                processed.insert(*id);
            }
        }
        if self.inner.index.read().await.retained_bytes > maximum_bytes {
            for (id, _) in candidates {
                if self.inner.index.read().await.retained_bytes <= low_watermark {
                    break;
                }
                if processed.insert(id) {
                    self.reclaim_retention_candidate(
                        id,
                        &mut reclaimed,
                        &mut pending_notifications,
                        &mut pending_lock_cleanup,
                    )
                    .await?;
                }
            }
        }
        if pending_notifications > 0 {
            self.publish_retention_batch(&mut pending_notifications, &mut pending_lock_cleanup)
                .await?;
        }
        Ok(reclaimed)
    }

    async fn reclaim_retention_candidate(
        &self,
        id: InvocationId,
        reclaimed: &mut usize,
        pending_notifications: &mut usize,
        pending_lock_cleanup: &mut Vec<InvocationId>,
    ) -> Result<(), StoreError> {
        let outcome = match self.reclaim_terminal(id).await {
            Ok(outcome) => outcome,
            Err(error) => {
                if *pending_notifications > 0 {
                    self.publish_retention_batch(pending_notifications, pending_lock_cleanup)
                        .await?;
                }
                return Err(error);
            }
        };
        *reclaimed += usize::from(outcome.reclaimed);
        if outcome.deleted {
            pending_lock_cleanup.push(id);
        }
        if outcome.changed {
            *pending_notifications += 1;
            if *pending_notifications >= GC_NOTIFICATION_BATCH_SIZE {
                self.publish_retention_batch(pending_notifications, pending_lock_cleanup)
                    .await?;
            }
        }
        Ok(())
    }

    async fn publish_retention_batch(
        &self,
        pending_notifications: &mut usize,
        pending_lock_cleanup: &mut Vec<InvocationId>,
    ) -> Result<(), StoreError> {
        let change = self.publish_change().await;
        let ids = std::mem::take(pending_lock_cleanup);
        let cache_root = self.cache_root.clone();
        tokio::task::spawn_blocking(move || {
            for id in ids {
                let _ = std::fs::remove_file(owner_lock_path(&cache_root, id));
                let _ = std::fs::remove_file(mutation_lock_path(&cache_root, id));
            }
        })
        .await?;
        *pending_notifications = 0;
        change?;
        Ok(())
    }

    async fn reclaim_terminal(&self, id: InvocationId) -> Result<ReclaimOutcome, StoreError> {
        let mutation_lock = self.mutation_lock(id).await;
        let _guard = mutation_lock.lock().await;
        let _process_mutation =
            ProcessLock::acquire(mutation_lock_path(&self.cache_root, id)).await?;
        let paths = self.paths_for_id(id);
        let (mut durable, manifest_bytes) = match read_durable(&paths.manifest).await {
            Ok(durable) => durable,
            Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ReclaimOutcome::default());
            }
            Err(error) => return Err(error),
        };
        if !durable.invocation.state.is_terminal() {
            return Ok(ReclaimOutcome::default());
        }
        let before = durable.payload_bytes.saturating_add(manifest_bytes);
        let currently_protected = durable.deferred.as_ref().is_some_and(|deferred| {
            !deferred.is_expired(
                bazel_mcp_types::unix_timestamp_ms(),
                durable.invocation.state.is_terminal(),
            )
        });
        if currently_protected {
            stage_raw_evidence(&self.cache_root, id, &paths).await?;
            let retained_bytes = self.persist_durable(&paths, &mut durable, true).await?;
            {
                let mut index = self.inner.index.write().await;
                replace_index_entry(&mut index, id, durable.index_entry(retained_bytes));
            }
            drop(_process_mutation);
            // The rename and manifest update committed pruning. Unlinking the
            // staged evidence is deliberately outside the shared index lock.
            let _ = finish_staged_evidence(&self.cache_root, id).await;
            return Ok(ReclaimOutcome {
                changed: true,
                deleted: false,
                reclaimed: retained_bytes < before,
            });
        }
        let rename_started = Instant::now();
        if let Some(trash) = rename_to_trash(&self.cache_root, id).await? {
            self.inner
                .metrics
                .record_gc_rename(elapsed_us(rename_started.elapsed()));
            {
                let index_started = Instant::now();
                let mut index = self.inner.index.write().await;
                remove_index_entry(&mut index, id);
                self.inner
                    .metrics
                    .record_gc_index_write(elapsed_us(index_started.elapsed()));
            }
            drop(_process_mutation);
            // Rename is the deletion commit. If unlinking fails, the index
            // stays removed and startup finishes this trash entry.
            let unlink_started = Instant::now();
            #[cfg(test)]
            let unlink_result = if self.inner.metrics.take_gc_unlink_failure() {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "injected GC unlink failure",
                ))
            } else {
                tokio::fs::remove_dir_all(trash).await
            };
            #[cfg(not(test))]
            let unlink_result = tokio::fs::remove_dir_all(trash).await;
            self.inner
                .metrics
                .record_gc_unlink(elapsed_us(unlink_started.elapsed()), unlink_result.is_ok());
            return Ok(ReclaimOutcome {
                changed: true,
                deleted: true,
                reclaimed: true,
            });
        }
        Ok(ReclaimOutcome::default())
    }
}
