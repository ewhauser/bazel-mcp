//! In-memory invocation indexes and telemetry reconciliation.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

use bazel_mcp_types::{DeferredResultRecord, InvocationId, InvocationMetrics};

use crate::{manifest::DurableRecord, record::InvocationHeader, storage::StoreError};

#[derive(Default)]
pub(crate) struct Index {
    pub(crate) entries: BTreeMap<InvocationId, IndexEntry>,
    pub(crate) by_requested: BTreeSet<(i64, InvocationId)>,
    pub(crate) by_workspace: BTreeMap<PathBuf, BTreeSet<(i64, InvocationId)>>,
    pub(crate) deferred_by_created: BTreeSet<(i64, InvocationId)>,
    pub(crate) terminal_by_finished: BTreeSet<(i64, InvocationId)>,
    pub(crate) retained_bytes: u64,
}

#[derive(Clone)]
pub(crate) struct IndexEntry {
    pub(crate) record: InvocationHeader,
    pub(crate) deferred: Option<DeferredResultRecord>,
    pub(crate) retained_bytes: u64,
    pub(crate) telemetry_generation: u64,
    pub(crate) telemetry_flush_scheduled: bool,
}

impl DurableRecord {
    pub(crate) fn index_entry(&self, retained_bytes: u64) -> IndexEntry {
        IndexEntry {
            record: self.invocation.clone(),
            deferred: self.deferred.clone(),
            retained_bytes,
            telemetry_generation: 0,
            telemetry_flush_scheduled: false,
        }
    }
}

pub(crate) fn ensure_exists(index: &Index, id: InvocationId) -> Result<(), StoreError> {
    index
        .entries
        .contains_key(&id)
        .then_some(())
        .ok_or(StoreError::NotFound(id))
}

pub(crate) fn insert(index: &mut Index, id: InvocationId, entry: IndexEntry) {
    add_secondary_indexes(index, id, &entry);
    index.retained_bytes = index.retained_bytes.saturating_add(entry.retained_bytes);
    index.entries.insert(id, entry);
}

pub(crate) fn replace(index: &mut Index, id: InvocationId, mut entry: IndexEntry) {
    if let Some(previous) = index.entries.remove(&id) {
        merge_telemetry(&previous.record.metrics, &mut entry.record.metrics);
        entry.telemetry_generation = previous.telemetry_generation;
        entry.telemetry_flush_scheduled = previous.telemetry_flush_scheduled;
        remove_secondary_indexes(index, id, &previous);
        index.retained_bytes = index.retained_bytes.saturating_sub(previous.retained_bytes);
    }
    add_secondary_indexes(index, id, &entry);
    index.retained_bytes = index.retained_bytes.saturating_add(entry.retained_bytes);
    index.entries.insert(id, entry);
}

pub(crate) fn remove(index: &mut Index, id: InvocationId) {
    if let Some(entry) = index.entries.remove(&id) {
        remove_secondary_indexes(index, id, &entry);
        index.retained_bytes = index.retained_bytes.saturating_sub(entry.retained_bytes);
    }
}

pub(crate) fn merge_index_telemetry(
    index: &Index,
    id: InvocationId,
    metrics: &mut InvocationMetrics,
) -> u64 {
    if let Some(entry) = index.entries.get(&id) {
        merge_telemetry(&entry.record.metrics, metrics);
        entry.telemetry_generation
    } else {
        0
    }
}

pub(crate) fn mark_telemetry_flushed(index: &mut Index, id: InvocationId, generation: u64) {
    if let Some(entry) = index.entries.get_mut(&id)
        && entry.telemetry_generation == generation
    {
        entry.telemetry_flush_scheduled = false;
    }
}

pub(crate) fn merge_telemetry(source: &InvocationMetrics, destination: &mut InvocationMetrics) {
    destination.model_visible_bytes = destination
        .model_visible_bytes
        .max(source.model_visible_bytes);
    destination.progress_notifications = destination
        .progress_notifications
        .max(source.progress_notifications);
    destination.inspect_calls = destination.inspect_calls.max(source.inspect_calls);
}

pub(crate) fn merge_pending_telemetry(previous: &Index, refreshed: &mut Index) {
    for (id, previous_entry) in &previous.entries {
        let Some(refreshed_entry) = refreshed.entries.get_mut(id) else {
            continue;
        };
        merge_telemetry(
            &previous_entry.record.metrics,
            &mut refreshed_entry.record.metrics,
        );
        refreshed_entry.telemetry_generation = previous_entry.telemetry_generation;
        refreshed_entry.telemetry_flush_scheduled = previous_entry.telemetry_flush_scheduled;
    }
}

fn add_secondary_indexes(index: &mut Index, id: InvocationId, entry: &IndexEntry) {
    let requested = (entry.record.request.requested_at_ms, id);
    index.by_requested.insert(requested);
    index
        .by_workspace
        .entry(entry.record.request.workspace.clone())
        .or_default()
        .insert(requested);
    if let Some(deferred) = &entry.deferred {
        index
            .deferred_by_created
            .insert((deferred.created_at_ms, id));
    }
    if entry.record.state.is_terminal() {
        index
            .terminal_by_finished
            .insert((entry.record.finished_at_ms.unwrap_or(i64::MIN), id));
    }
}

fn remove_secondary_indexes(index: &mut Index, id: InvocationId, entry: &IndexEntry) {
    let requested = (entry.record.request.requested_at_ms, id);
    index.by_requested.remove(&requested);
    let workspace = entry.record.request.workspace.clone();
    let remove_workspace = index
        .by_workspace
        .get_mut(&workspace)
        .is_some_and(|entries| {
            entries.remove(&requested);
            entries.is_empty()
        });
    if remove_workspace {
        index.by_workspace.remove(&workspace);
    }
    if let Some(deferred) = &entry.deferred {
        index
            .deferred_by_created
            .remove(&(deferred.created_at_ms, id));
    }
    if entry.record.state.is_terminal() {
        index
            .terminal_by_finished
            .remove(&(entry.record.finished_at_ms.unwrap_or(i64::MIN), id));
    }
}
