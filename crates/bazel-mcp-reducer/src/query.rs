use bazel_mcp_types::QueryRow;

use crate::{Budget, Budgeted};

#[must_use]
pub fn reduce_query(input: &[u8], budget: Budget) -> Budgeted<Vec<QueryRow>> {
    let text = String::from_utf8_lossy(input);
    let mut bytes = 0_usize;
    let mut truncated = false;
    let mut rows = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if rows.len() >= budget.max_items || bytes.saturating_add(line.len()) > budget.max_bytes {
            truncated = true;
            break;
        }
        bytes += line.len();
        rows.push(QueryRow {
            ordinal: index as u64,
            value: line.to_owned(),
        });
    }
    Budgeted {
        value: rows,
        truncated,
    }
}
