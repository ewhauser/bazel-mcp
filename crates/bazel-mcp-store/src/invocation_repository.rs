//! Compact invocation lookup and listing, plus explicit detail hydration.

use std::{collections::BTreeSet, path::Path};

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
        let limit = page.limit.clamp(1, 200) as usize;
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
        let collect = |ordered: &BTreeSet<(i64, InvocationId)>| {
            ordered
                .iter()
                .rev()
                .filter(|(requested_at_ms, id)| {
                    cursor.as_ref().is_none_or(|cursor| {
                        *requested_at_ms < cursor.requested_at_ms
                            || (*requested_at_ms == cursor.requested_at_ms
                                && id.to_string() < cursor.id)
                    })
                })
                .filter_map(|(_, id)| index.entries.get(id))
                .filter(|entry| state.is_none_or(|state| entry.record.state == state))
                .filter(|entry| {
                    command.is_none_or(|command| entry.record.request.command == *command)
                })
                .map(|entry| entry.record.clone())
                .take(limit + 1)
                .collect::<Vec<_>>()
        };
        let mut items = if let Some(workspace) = workspace {
            index
                .by_workspace
                .get(workspace)
                .map_or_else(Vec::new, collect)
        } else {
            collect(&index.by_requested)
        };
        let truncated = items.len() > limit;
        items.truncate(limit);
        let next_cursor = if truncated {
            items
                .last()
                .map(|record| {
                    InvocationCursor::new(
                        workspace_text.as_deref(),
                        state_text,
                        command_text,
                        record.request.requested_at_ms,
                        record.request.id.to_string(),
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
}
