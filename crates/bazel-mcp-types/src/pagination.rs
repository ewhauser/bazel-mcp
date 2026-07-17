use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PageRequest {
    pub cursor: Option<String>,
    /// Maximum matching records returned to the caller.
    pub item_limit: u32,
    /// Maximum source records examined while producing one page. This keeps
    /// filtering work bounded independently of representation encoding.
    pub scan_limit: u32,
}

impl Default for PageRequest {
    fn default() -> Self {
        Self {
            cursor: None,
            item_limit: 50,
            scan_limit: 1_000,
        }
    }
}

impl PageRequest {
    #[must_use]
    pub fn new(cursor: Option<String>, item_limit: u32) -> Self {
        Self {
            cursor,
            item_limit,
            ..Self::default()
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    /// Total source records when known without violating the scan limit.
    pub total_count: Option<u64>,
    /// Total matching records when known without violating the scan limit.
    pub filtered_count: Option<u64>,
    pub next_cursor: Option<String>,
    pub truncated: bool,
    /// Continuation after each returned item. This stays inside the
    /// application boundary so a final encoder can pack fewer complete items
    /// without creating cursor gaps or duplicates.
    #[serde(default, skip)]
    pub item_cursors: Vec<String>,
}
