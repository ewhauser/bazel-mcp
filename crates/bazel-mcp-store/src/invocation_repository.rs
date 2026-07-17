//! Compact invocation lookup and listing, plus explicit detail hydration.

use std::path::Path;

use bazel_mcp_types::{
    BazelCommand, InvocationId, InvocationRecord, InvocationState, Page, PageRequest,
};

use crate::{
    cursor::InvocationCursor,
    record::{HydratedInvocation, InvocationHeader},
    storage::{Store, StoreError},
};

impl Store {
    /// Read the compact manifest/index representation without loading details.
    pub async fn get_invocation_header(
        &self,
        id: InvocationId,
    ) -> Result<InvocationHeader, StoreError> {
        self.ensure_invocation(id).await?;
        self.inner
            .index
            .read()
            .await
            .entries
            .get(&id)
            .map(|entry| entry.record.clone())
            .ok_or(StoreError::NotFound(id))
    }

    /// Read one complete invocation, hydrating target, test, and coverage
    /// collections from the detail sidecar.
    pub async fn get_hydrated_invocation(
        &self,
        id: InvocationId,
    ) -> Result<HydratedInvocation, StoreError> {
        self.read_hydrated_invocation(id).await
    }

    /// Compatibility projection of [`Self::get_hydrated_invocation`] into the
    /// original invocation record type.
    pub async fn get_invocation(&self, id: InvocationId) -> Result<InvocationRecord, StoreError> {
        Ok(self.get_hydrated_invocation(id).await?.into_record())
    }

    pub async fn list_invocations(
        &self,
        workspace: Option<&Path>,
        state: Option<InvocationState>,
        command: Option<&BazelCommand>,
        page: PageRequest,
    ) -> Result<Page<InvocationHeader>, StoreError> {
        self.refresh_index_if_stale().await?;
        let item_limit = page.item_limit.clamp(1, 200) as usize;
        let scan_limit = page.scan_limit.clamp(page.item_limit.max(1), 20_000) as usize;
        let workspace_text = workspace.map(|path| path.to_string_lossy().into_owned());
        let state_text = state.map(InvocationState::as_str);
        let command_text = command.map(BazelCommand::as_str);
        let cursor = page
            .cursor
            .as_deref()
            .map(|value| {
                InvocationCursor::decode_for(
                    value,
                    workspace_text.as_deref(),
                    state_text,
                    command_text,
                )
            })
            .transpose()?;
        let index = self.inner.index.read().await;
        let ordered = if let Some(workspace) = workspace {
            index
                .by_workspace
                .get(workspace)
                .map_or_else(Vec::new, |ordered| ordered.iter().rev().copied().collect())
        } else {
            index.by_requested.iter().rev().copied().collect()
        };
        let mut items = Vec::with_capacity(item_limit);
        let mut item_cursors = Vec::with_capacity(item_limit);
        let mut continuation = None;
        let mut scanned = 0_usize;
        let mut truncated = false;
        for (requested_at_ms, id) in ordered {
            if !cursor.as_ref().is_none_or(|cursor| {
                requested_at_ms < cursor.requested_at_ms
                    || (requested_at_ms == cursor.requested_at_ms && id.to_string() < cursor.id)
            }) {
                continue;
            }
            if scanned == scan_limit {
                truncated = true;
                break;
            }
            let Some(entry) = index.entries.get(&id) else {
                continue;
            };
            let matches = state.is_none_or(|state| entry.record.state == state)
                && command.is_none_or(|command| entry.record.request.command == *command);
            if matches && items.len() == item_limit {
                truncated = true;
                break;
            }
            scanned = scanned.saturating_add(1);
            let cursor = InvocationCursor::new(
                workspace_text.as_deref(),
                state_text,
                command_text,
                requested_at_ms,
                id.to_string(),
            )
            .encode()?;
            continuation = Some(cursor.clone());
            if matches {
                items.push(entry.record.clone());
                item_cursors.push(cursor);
            }
        }
        Ok(Page {
            items,
            total_count: None,
            filtered_count: None,
            next_cursor: if truncated { continuation } else { None },
            truncated,
            item_cursors,
        })
    }
}
