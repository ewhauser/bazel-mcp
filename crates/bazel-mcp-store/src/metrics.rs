//! Store I/O telemetry accumulation.

use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(test)]
use std::sync::atomic::AtomicBool;

use crate::StoreIoStats;

pub(crate) struct StoreMetrics {
    manifest_commits: AtomicU64,
    manifest_bytes_written: AtomicU64,
    payload_recounts: AtomicU64,
    gc_renames: AtomicU64,
    gc_unlinks: AtomicU64,
    gc_rename_us: AtomicU64,
    gc_index_write_us: AtomicU64,
    gc_unlink_us: AtomicU64,
    #[cfg(test)]
    fail_next_gc_unlink: AtomicBool,
}

impl StoreMetrics {
    pub(crate) fn new() -> Self {
        Self {
            manifest_commits: AtomicU64::new(0),
            manifest_bytes_written: AtomicU64::new(0),
            payload_recounts: AtomicU64::new(0),
            gc_renames: AtomicU64::new(0),
            gc_unlinks: AtomicU64::new(0),
            gc_rename_us: AtomicU64::new(0),
            gc_index_write_us: AtomicU64::new(0),
            gc_unlink_us: AtomicU64::new(0),
            #[cfg(test)]
            fail_next_gc_unlink: AtomicBool::new(false),
        }
    }

    pub(crate) fn record_manifest_commit(&self, recount_payload: bool) {
        self.manifest_commits.fetch_add(1, Ordering::Relaxed);
        if recount_payload {
            self.payload_recounts.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn record_manifest_bytes(&self, bytes: u64) {
        self.manifest_bytes_written
            .fetch_add(bytes, Ordering::Relaxed);
    }

    pub(crate) fn record_gc_rename(&self, elapsed_us: u64) {
        self.gc_renames.fetch_add(1, Ordering::Relaxed);
        self.gc_rename_us.fetch_add(elapsed_us, Ordering::Relaxed);
    }

    pub(crate) fn record_gc_index_write(&self, elapsed_us: u64) {
        self.gc_index_write_us
            .fetch_add(elapsed_us, Ordering::Relaxed);
    }

    pub(crate) fn record_gc_unlink(&self, elapsed_us: u64, succeeded: bool) {
        if succeeded {
            self.gc_unlinks.fetch_add(1, Ordering::Relaxed);
        }
        self.gc_unlink_us.fetch_add(elapsed_us, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn inject_gc_unlink_failure(&self) {
        self.fail_next_gc_unlink.store(true, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn take_gc_unlink_failure(&self) -> bool {
        self.fail_next_gc_unlink.swap(false, Ordering::Relaxed)
    }

    pub(crate) fn snapshot(&self) -> StoreIoStats {
        StoreIoStats {
            manifest_commits: self.manifest_commits.load(Ordering::Relaxed),
            manifest_bytes_written: self.manifest_bytes_written.load(Ordering::Relaxed),
            payload_recounts: self.payload_recounts.load(Ordering::Relaxed),
            gc_renames: self.gc_renames.load(Ordering::Relaxed),
            gc_unlinks: self.gc_unlinks.load(Ordering::Relaxed),
            gc_rename_us: self.gc_rename_us.load(Ordering::Relaxed),
            gc_index_write_us: self.gc_index_write_us.load(Ordering::Relaxed),
            gc_unlink_us: self.gc_unlink_us.load(Ordering::Relaxed),
        }
    }
}
