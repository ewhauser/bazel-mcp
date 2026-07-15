//! Durable Turso index and private invocation evidence files.

mod cursor;
mod database;
mod files;

pub use database::{Store, StoreError};
pub use files::InvocationPaths;

/// Parser entry point used by adversarial cursor tests and fuzzing.
#[must_use]
pub fn cursor_is_well_formed(value: &str) -> bool {
    cursor::InvocationCursor::decode(value).is_ok() || cursor::OrdinalCursor::decode(value).is_ok()
}
