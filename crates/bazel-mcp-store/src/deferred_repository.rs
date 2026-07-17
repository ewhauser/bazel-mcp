//! Deferred-result lookup, paging, mutation, and expiry.

use bazel_mcp_types::{
    DeferredFailure, DeferredResultView, DeferredRetrieval, DeferredTerminalState, InvocationId,
    Page, PageRequest,
};

use crate::{
    cursor::DeferredCursor,
    storage::{Store, StoreError},
};

impl Store {
    pub async fn get_deferred_result(
        &self,
        id: InvocationId,
        retrieval: DeferredRetrieval,
        now_ms: i64,
    ) -> Result<DeferredResultView, StoreError> {
        self.ensure_invocation(id).await?;
        let (deferred, record) = {
            let index = self.inner.index.read().await;
            let entry = index.entries.get(&id).ok_or(StoreError::NotFound(id))?;
            let Some(deferred) = entry.deferred.clone() else {
                return Err(StoreError::DeferredNotFound(id));
            };
            (deferred, entry.record.clone())
        };
        if deferred.retrieval != retrieval
            || deferred.is_expired(now_ms, record.state.is_terminal())
        {
            if deferred.is_expired(now_ms, record.state.is_terminal()) {
                self.mutate(id, false, |durable| {
                    durable.deferred = None;
                    Ok(())
                })
                .await?;
            }
            return Err(StoreError::DeferredNotFound(id));
        }
        let invocation = if record.state.is_terminal() {
            self.read_hydrated_invocation(id).await?.into_record()
        } else {
            record.into_record()
        };
        Ok(DeferredResultView {
            deferred,
            invocation,
        })
    }

    pub async fn list_deferred_results(
        &self,
        retrieval: DeferredRetrieval,
        now_ms: i64,
        page: PageRequest,
    ) -> Result<Page<DeferredResultView>, StoreError> {
        self.refresh_index_if_stale().await?;
        let limit = page.limit.clamp(1, 200) as usize;
        let cursor = page
            .cursor
            .as_deref()
            .map(|value| DeferredCursor::decode_for(value, retrieval.as_str()))
            .transpose()?;
        let index = self.inner.index.read().await;
        let mut items: Vec<_> = index
            .deferred_by_created
            .iter()
            .rev()
            .filter_map(|(_, id)| {
                let entry = index.entries.get(id)?;
                let deferred = entry.deferred.as_ref()?;
                (deferred.retrieval == retrieval
                    && !deferred.is_expired(now_ms, entry.record.state.is_terminal())
                    && cursor.as_ref().is_none_or(|cursor| {
                        deferred.created_at_ms < cursor.created_at_ms
                            || (deferred.created_at_ms == cursor.created_at_ms
                                && deferred.invocation_id.to_string() < cursor.id)
                    }))
                .then(|| DeferredResultView {
                    deferred: deferred.clone(),
                    invocation: entry.record.clone().into_record(),
                })
            })
            .take(limit + 1)
            .collect();
        let truncated = items.len() > limit;
        items.truncate(limit);
        let next_cursor = if truncated {
            items
                .last()
                .map(|view| {
                    DeferredCursor::new(
                        retrieval.as_str(),
                        view.deferred.created_at_ms,
                        view.deferred.invocation_id.to_string(),
                    )
                    .encode()
                })
                .transpose()?
        } else {
            None
        };
        Ok(Page {
            items,
            next_cursor,
            truncated,
        })
    }

    pub async fn record_deferred_cancellation(
        &self,
        id: InvocationId,
        requested_at_ms: i64,
    ) -> Result<(), StoreError> {
        self.mutate(id, false, |durable| {
            let deferred = durable
                .deferred
                .as_mut()
                .ok_or(StoreError::DeferredNotFound(id))?;
            if deferred.cancellation_requested_at_ms.is_none() {
                deferred.cancellation_requested_at_ms = Some(requested_at_ms);
            }
            deferred.updated_at_ms = deferred.updated_at_ms.max(requested_at_ms);
            Ok(())
        })
        .await
    }

    pub async fn set_deferred_terminal_override(
        &self,
        id: InvocationId,
        state: DeferredTerminalState,
        updated_at_ms: i64,
    ) -> Result<(), StoreError> {
        self.mutate(id, false, |durable| {
            let deferred = durable
                .deferred
                .as_mut()
                .ok_or(StoreError::DeferredNotFound(id))?;
            deferred.terminal_override = Some(state);
            deferred.updated_at_ms = deferred.updated_at_ms.max(updated_at_ms);
            Ok(())
        })
        .await
    }

    pub async fn persist_deferred_failure(
        &self,
        id: InvocationId,
        failure: &DeferredFailure,
        updated_at_ms: i64,
    ) -> Result<(), StoreError> {
        self.mutate(id, false, |durable| {
            let deferred = durable
                .deferred
                .as_mut()
                .ok_or(StoreError::DeferredNotFound(id))?;
            deferred.failure = Some(failure.clone());
            deferred.updated_at_ms = deferred.updated_at_ms.max(updated_at_ms);
            Ok(())
        })
        .await
    }

    pub async fn extend_deferred_expiry(
        &self,
        id: InvocationId,
        minimum_expires_at_ms: i64,
        updated_at_ms: i64,
    ) -> Result<(), StoreError> {
        self.mutate(id, false, |durable| {
            let deferred = durable
                .deferred
                .as_mut()
                .ok_or(StoreError::DeferredNotFound(id))?;
            deferred.expires_at_ms = deferred.expires_at_ms.max(minimum_expires_at_ms);
            deferred.updated_at_ms = deferred.updated_at_ms.max(updated_at_ms);
            Ok(())
        })
        .await
    }

    pub async fn delete_expired_deferred_results(&self, now_ms: i64) -> Result<usize, StoreError> {
        self.refresh_index_if_stale().await?;
        let ids: Vec<_> = {
            let index = self.inner.index.read().await;
            index
                .entries
                .iter()
                .filter_map(|(id, entry)| {
                    entry
                        .deferred
                        .as_ref()
                        .is_some_and(|deferred| {
                            deferred.is_expired(now_ms, entry.record.state.is_terminal())
                        })
                        .then_some(*id)
                })
                .collect()
        };
        let mut deleted = 0;
        for id in &ids {
            let mut removed = false;
            self.mutate(*id, false, |durable| {
                if durable.deferred.as_ref().is_some_and(|deferred| {
                    deferred.is_expired(now_ms, durable.invocation.state.is_terminal())
                }) {
                    durable.deferred = None;
                    removed = true;
                }
                Ok(())
            })
            .await?;
            deleted += usize::from(removed);
        }
        Ok(deleted)
    }
}
