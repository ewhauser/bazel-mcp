//! Database-free, crash-recoverable invocation evidence storage.

mod cursor;
mod deferred_repository;
mod files;
mod index;
mod invocation_repository;
mod manifest;
mod query_paging;
mod record;
mod retention;
mod storage;

pub use files::InvocationPaths;
pub use record::{
    CoverageHeader, HydratedInvocation, InvocationDetails, InvocationHeader,
    InvocationSummaryHeader,
};
pub use storage::{InvocationCompletion, Store, StoreError, StoreIoStats, StoreStartupStats};

/// Parser entry point used by adversarial cursor tests and fuzzing.
#[must_use]
pub fn cursor_is_well_formed(value: &str) -> bool {
    cursor::InvocationCursor::decode(value).is_ok()
        || cursor::OrdinalCursor::decode(value).is_ok()
        || cursor::FileCursor::decode(value).is_ok()
}
