//! Bounded pagination for in-memory records and query-result files.

use std::{
    borrow::Cow,
    fs::File,
    io::{BufRead, BufReader},
    path::Path,
};

use bazel_mcp_types::{InvocationId, Page, PageRequest, QueryRow};

use crate::{
    cursor::{FileCursor, OrdinalCursor},
    storage::StoreError,
};

pub(crate) const QUERY_LINE_LIMIT: usize = 64 * 1024;
const QUERY_COUNT_BUFFER_BYTES: usize = 64 * 1024;

pub(crate) fn count_query_file(path: &Path) -> Result<u64, StoreError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    count_query_rows(&file)
}

fn count_query_rows(mut file: &File) -> Result<u64, StoreError> {
    use std::io::{Seek, SeekFrom};

    file.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::with_capacity(QUERY_COUNT_BUFFER_BYTES, file);
    let mut rows = 0_u64;
    let mut saw_bytes = false;
    let mut last_byte = b'\n';
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            break;
        }
        saw_bytes = true;
        last_byte = buffer[buffer.len() - 1];
        rows = rows.saturating_add(
            u64::try_from(memchr::memchr_iter(b'\n', buffer).count()).unwrap_or(u64::MAX),
        );
        let consumed = buffer.len();
        reader.consume(consumed);
    }
    if saw_bytes && last_byte != b'\n' {
        rows = rows.saturating_add(1);
    }
    Ok(rows)
}

pub(crate) fn page_records<T, F>(
    scope: &str,
    id: InvocationId,
    filter: Option<&str>,
    page: PageRequest,
    records: Vec<T>,
    searchable: F,
) -> Result<Page<T>, StoreError>
where
    F: Fn(&T) -> String,
{
    let item_limit = page.item_limit.clamp(1, 100) as usize;
    let scan_limit = page.scan_limit.clamp(page.item_limit.max(1), 10_000) as usize;
    let invocation_id = id.to_string();
    let after = page
        .cursor
        .as_deref()
        .map(|value| OrdinalCursor::decode_for(value, scope, &invocation_id, filter))
        .transpose()?
        .map_or(-1, |cursor| cursor.ordinal);
    let normalized_filter = filter.map(str::to_ascii_lowercase);
    let total = records.len() as u64;
    let started_at_beginning = after < 0;
    let mut filtered = 0_u64;
    let mut selected = Vec::new();
    let mut item_cursors = Vec::new();
    let mut last_scanned = None;
    let mut scanned = 0_usize;
    let mut truncated = false;
    for (ordinal, record) in records.into_iter().enumerate() {
        let ordinal = i64::try_from(ordinal).unwrap_or(i64::MAX);
        if ordinal <= after {
            continue;
        }
        if scanned == scan_limit {
            truncated = true;
            break;
        }
        let matches = normalized_filter
            .as_ref()
            .is_none_or(|filter| searchable(&record).to_ascii_lowercase().contains(filter));
        filtered = filtered.saturating_add(u64::from(matches));
        if matches && selected.len() == item_limit {
            truncated = true;
            break;
        }
        scanned = scanned.saturating_add(1);
        last_scanned = Some(ordinal);
        if matches {
            item_cursors.push(OrdinalCursor::new(scope, &invocation_id, filter, ordinal).encode()?);
            selected.push(record);
        }
    }
    let next_cursor = if truncated {
        last_scanned
            .map(|ordinal| OrdinalCursor::new(scope, &invocation_id, filter, ordinal).encode())
            .transpose()?
    } else {
        None
    };
    Ok(Page {
        items: selected,
        total_count: Some(total),
        filtered_count: if normalized_filter.is_none() {
            Some(total)
        } else if started_at_beginning && !truncated {
            Some(filtered)
        } else {
            None
        },
        next_cursor,
        truncated,
        item_cursors,
    })
}

pub(crate) struct QueryFilePage {
    pub(crate) start_offset: u64,
    pub(crate) start_ordinal: u64,
    pub(crate) prior_total: u64,
    pub(crate) prior_filtered: u64,
    pub(crate) item_limit: usize,
    pub(crate) scan_limit: usize,
    pub(crate) known_total: Option<u64>,
}

pub(crate) fn page_query_file<F>(
    path: &Path,
    invocation_id: &str,
    filter: Option<&str>,
    request: QueryFilePage,
    transform: F,
) -> Result<Page<QueryRow>, StoreError>
where
    F: Fn(&str, &mut String),
{
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Page {
                items: Vec::new(),
                total_count: Some(0),
                filtered_count: Some(0),
                next_cursor: None,
                truncated: false,
                item_cursors: Vec::new(),
            });
        }
        Err(error) => return Err(error.into()),
    };
    page_query_file_from_cursor(file, invocation_id, filter, request, transform)
}

fn page_query_file_from_cursor<F>(
    mut file: File,
    invocation_id: &str,
    filter: Option<&str>,
    request: QueryFilePage,
    transform: F,
) -> Result<Page<QueryRow>, StoreError>
where
    F: Fn(&str, &mut String),
{
    use std::io::{Seek, SeekFrom};

    file.seek(SeekFrom::Start(request.start_offset))?;
    let mut reader = BoundedLineReader::with_position(
        BufReader::new(file),
        QUERY_LINE_LIMIT,
        request.start_offset,
        request.start_ordinal,
    );
    let mut total = request.prior_total;
    let mut filtered = request.prior_filtered;
    let mut selected = Vec::with_capacity(request.item_limit);
    let mut item_cursors = Vec::with_capacity(request.item_limit);
    let mut truncated = false;
    let mut scan_exhausted = false;
    let mut transformed = String::new();
    let mut continuation = None;
    let mut scanned = 0_usize;
    while let Some(line) = reader.next_line()? {
        if scanned == request.scan_limit {
            truncated = true;
            scan_exhausted = true;
            break;
        }
        if filter.is_none() && selected.len() == request.item_limit {
            if let Some(known_total) = request.known_total {
                truncated = total < known_total;
                break;
            }
            scanned = scanned.saturating_add(1);
            total = total.saturating_add(1);
            filtered = filtered.saturating_add(1);
            truncated = true;
            continue;
        }
        transform(line.value.as_ref(), &mut transformed);
        let matches = filter.is_none_or(|filter| contains_ignore_ascii_case(&transformed, filter));
        scanned = scanned.saturating_add(1);
        total = total.saturating_add(1);
        if matches {
            filtered = filtered.saturating_add(1);
            if selected.len() == request.item_limit {
                truncated = true;
                continue;
            }
            let item_cursor = FileCursor::new(
                "query_rows",
                invocation_id,
                filter,
                line.end_offset,
                line.ordinal,
                total,
                filtered,
            );
            item_cursors.push(item_cursor.encode()?);
            selected.push(QueryRow {
                ordinal: line.ordinal,
                value: std::mem::take(&mut transformed),
            });
            continuation = Some((line.end_offset, line.ordinal, total, filtered));
        } else if selected.len() < request.item_limit {
            continuation = Some((line.end_offset, line.ordinal, total, filtered));
        }
    }
    let total_count = if let Some(known_total) = request.known_total {
        Some(known_total.max(total))
    } else if scan_exhausted {
        None
    } else {
        Some(total)
    };
    let filtered_count = if filter.is_none() {
        total_count
    } else if scan_exhausted {
        None
    } else {
        Some(filtered)
    };
    Ok(Page {
        items: selected,
        total_count,
        filtered_count,
        next_cursor: if truncated {
            continuation
                .map(|(offset, ordinal, total, filtered)| {
                    FileCursor::new(
                        "query_rows",
                        invocation_id,
                        filter,
                        offset,
                        ordinal,
                        total,
                        filtered,
                    )
                    .encode()
                })
                .transpose()?
        } else {
            None
        },
        truncated,
        item_cursors,
    })
}

fn contains_ignore_ascii_case(value: &str, needle: &str) -> bool {
    let needle = needle.as_bytes();
    needle.is_empty()
        || value
            .as_bytes()
            .windows(needle.len())
            .any(|window| window.eq_ignore_ascii_case(needle))
}

struct BoundedLineReader<R> {
    reader: R,
    offset: u64,
    ordinal: u64,
    limit: usize,
    value: Vec<u8>,
}

struct BoundedLine<'a> {
    end_offset: u64,
    ordinal: u64,
    value: Cow<'a, str>,
}

impl<R: BufRead> BoundedLineReader<R> {
    fn with_position(reader: R, limit: usize, offset: u64, ordinal: u64) -> Self {
        Self {
            reader,
            offset,
            ordinal,
            limit,
            value: Vec::new(),
        }
    }

    fn next_line(&mut self) -> std::io::Result<Option<BoundedLine<'_>>> {
        let ordinal = self.ordinal;
        self.value.clear();
        let mut saw_bytes = false;
        loop {
            let (consumed, newline, reached_eof) = {
                let available = self.reader.fill_buf()?;
                if available.is_empty() {
                    (0, false, true)
                } else if let Some(position) = available.iter().position(|byte| *byte == b'\n') {
                    let consumed = position + 1;
                    let copy = position.min(self.limit.saturating_sub(self.value.len()));
                    self.value.extend_from_slice(&available[..copy]);
                    (consumed, true, false)
                } else {
                    let consumed = available.len();
                    let copy = consumed.min(self.limit.saturating_sub(self.value.len()));
                    self.value.extend_from_slice(&available[..copy]);
                    (consumed, false, false)
                }
            };
            if reached_eof {
                if !saw_bytes {
                    return Ok(None);
                }
                break;
            }
            saw_bytes = true;
            self.reader.consume(consumed);
            self.offset = self
                .offset
                .saturating_add(u64::try_from(consumed).unwrap_or(u64::MAX));
            if newline {
                break;
            }
        }
        if self.value.last() == Some(&b'\r') {
            self.value.pop();
        }
        self.ordinal = self.ordinal.saturating_add(1);
        Ok(Some(BoundedLine {
            end_offset: self.offset,
            ordinal,
            value: String::from_utf8_lossy(&self.value),
        }))
    }
}
