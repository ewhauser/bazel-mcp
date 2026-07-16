use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PageRequest {
    pub cursor: Option<String>,
    pub limit: u32,
    /// Optional serialized item-array budget. The storage layer packs complete
    /// records up to this limit and advances cursors only past emitted records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<usize>,
}

impl Default for PageRequest {
    fn default() -> Self {
        Self {
            cursor: None,
            limit: 50,
            max_bytes: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
    pub truncated: bool,
}
