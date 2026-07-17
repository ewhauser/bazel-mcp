//! Bounded pagination for in-memory records and query-result files.

use std::{
    borrow::Cow,
    fs::File,
    io::{BufRead, BufReader},
    path::Path,
};

use bazel_mcp_types::{InvocationId, Page, PageRequest, QueryRow};
use serde::Serialize;

use crate::{
    cursor::{FileCursor, OrdinalCursor},
    storage::StoreError,
};

pub(crate) const QUERY_LINE_LIMIT: usize = 64 * 1024;

pub(crate) fn page_records<T, F>(
    scope: &str,
    id: InvocationId,
    filter: Option<&str>,
    page: PageRequest,
    records: Vec<T>,
    searchable: F,
) -> Result<(Page<T>, u64, u64), StoreError>
where
    T: Serialize,
    F: Fn(&T) -> String,
{
    let limit = page.limit.clamp(1, 100) as usize;
    let maximum_bytes = page.max_bytes.unwrap_or(usize::MAX);
    let invocation_id = id.to_string();
    let after = page
        .cursor
        .as_deref()
        .map(|value| OrdinalCursor::decode_for(value, scope, &invocation_id, filter))
        .transpose()?
        .map_or(-1, |cursor| cursor.ordinal);
    let normalized_filter = filter.map(str::to_ascii_lowercase);
    let total = records.len() as u64;
    let filtered = records
        .iter()
        .filter(|record| {
            normalized_filter
                .as_ref()
                .is_none_or(|filter| searchable(record).to_ascii_lowercase().contains(filter))
        })
        .count() as u64;
    let mut selected = Vec::new();
    let mut used_bytes = 2_usize;
    let mut last_ordinal = None;
    let mut truncated = false;
    for (ordinal, record) in records.into_iter().enumerate() {
        let ordinal = i64::try_from(ordinal).unwrap_or(i64::MAX);
        if ordinal <= after
            || !normalized_filter
                .as_ref()
                .is_none_or(|filter| searchable(&record).to_ascii_lowercase().contains(filter))
        {
            continue;
        }
        let item_bytes = serde_json::to_vec(&record)?.len();
        let separator = usize::from(!selected.is_empty());
        if selected.len() == limit
            || (!selected.is_empty()
                && used_bytes
                    .saturating_add(separator)
                    .saturating_add(item_bytes)
                    > maximum_bytes)
        {
            truncated = true;
            break;
        }
        used_bytes = used_bytes
            .saturating_add(separator)
            .saturating_add(item_bytes);
        last_ordinal = Some(ordinal);
        selected.push(record);
    }
    let next_cursor = if truncated {
        last_ordinal
            .map(|ordinal| OrdinalCursor::new(scope, &invocation_id, filter, ordinal).encode())
            .transpose()?
    } else {
        None
    };
    Ok((
        Page {
            items: selected,
            next_cursor,
            truncated,
        },
        total,
        filtered,
    ))
}

pub(crate) struct QueryFilePage {
    pub(crate) start_offset: u64,
    pub(crate) start_ordinal: u64,
    pub(crate) limit: usize,
    pub(crate) maximum_bytes: usize,
    pub(crate) known_total: Option<u64>,
}

pub(crate) fn page_query_file<F>(
    path: &Path,
    invocation_id: &str,
    filter: Option<&str>,
    request: QueryFilePage,
    transform: F,
) -> Result<(Page<QueryRow>, u64, u64), StoreError>
where
    F: Fn(&str, &mut String),
{
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((
                Page {
                    items: Vec::new(),
                    next_cursor: None,
                    truncated: false,
                },
                0,
                0,
            ));
        }
        Err(error) => return Err(error.into()),
    };
    if filter.is_none() {
        let total = if let Some(total) = request.known_total {
            total
        } else {
            count_query_rows(&file)?
        };
        return page_unfiltered_query_file(file, invocation_id, request, total, transform);
    }
    let mut reader = BoundedLineReader::new(BufReader::new(file), QUERY_LINE_LIMIT);
    let mut total = 0_u64;
    let mut filtered = 0_u64;
    let mut selected = Vec::with_capacity(request.limit);
    let mut used_bytes = 2_usize;
    let mut truncated = false;
    let mut transformed = String::new();
    let mut serialized = Vec::new();
    while let Some(line) = reader.next_line()? {
        total = total.saturating_add(1);
        transform(line.value.as_ref(), &mut transformed);
        let matches = filter.is_none_or(|filter| contains_ignore_ascii_case(&transformed, filter));
        if matches {
            filtered = filtered.saturating_add(1);
            if line.start_offset >= request.start_offset && !truncated {
                if selected.len() == request.limit {
                    truncated = true;
                } else {
                    let item_bytes =
                        serialized_query_row_len(line.ordinal, &transformed, &mut serialized)?;
                    let separator = usize::from(!selected.is_empty());
                    if !selected.is_empty()
                        && used_bytes
                            .saturating_add(separator)
                            .saturating_add(item_bytes)
                            > request.maximum_bytes
                    {
                        truncated = true;
                    } else {
                        used_bytes = used_bytes
                            .saturating_add(separator)
                            .saturating_add(item_bytes);
                        selected.push(SelectedQueryLine {
                            end_offset: line.end_offset,
                            ordinal: line.ordinal,
                            value: std::mem::take(&mut transformed),
                        });
                    }
                }
            }
        }
    }
    let next_cursor = if truncated {
        selected
            .last()
            .map(|line| {
                FileCursor::new(
                    "query_rows",
                    invocation_id,
                    filter,
                    line.end_offset,
                    line.ordinal,
                )
                .encode()
            })
            .transpose()?
    } else {
        None
    };
    Ok((
        Page {
            items: selected
                .into_iter()
                .map(|line| QueryRow {
                    ordinal: line.ordinal,
                    value: line.value,
                })
                .collect(),
            next_cursor,
            truncated,
        },
        total,
        filtered,
    ))
}

fn count_query_rows(mut file: &File) -> Result<u64, StoreError> {
    use std::io::{Read, Seek, SeekFrom};

    file.seek(SeekFrom::Start(0))?;
    let mut buffer = vec![0_u8; 1024 * 1024];
    let mut rows = 0_u64;
    let mut saw_bytes = false;
    let mut last_byte = b'\n';
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        saw_bytes = true;
        last_byte = buffer[read - 1];
        rows = rows.saturating_add(
            u64::try_from(memchr::memchr_iter(b'\n', &buffer[..read]).count()).unwrap_or(u64::MAX),
        );
    }
    if saw_bytes && last_byte != b'\n' {
        rows = rows.saturating_add(1);
    }
    Ok(rows)
}

fn page_unfiltered_query_file<F>(
    mut file: File,
    invocation_id: &str,
    request: QueryFilePage,
    total: u64,
    transform: F,
) -> Result<(Page<QueryRow>, u64, u64), StoreError>
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
    let mut selected = Vec::with_capacity(request.limit);
    let mut used_bytes = 2_usize;
    let mut truncated = false;
    let mut transformed = String::new();
    let mut serialized = Vec::new();
    while let Some(line) = reader.next_line()? {
        if selected.len() == request.limit {
            truncated = true;
            break;
        }
        transform(line.value.as_ref(), &mut transformed);
        let item_bytes = serialized_query_row_len(line.ordinal, &transformed, &mut serialized)?;
        let separator = usize::from(!selected.is_empty());
        if !selected.is_empty()
            && used_bytes
                .saturating_add(separator)
                .saturating_add(item_bytes)
                > request.maximum_bytes
        {
            truncated = true;
            break;
        }
        used_bytes = used_bytes
            .saturating_add(separator)
            .saturating_add(item_bytes);
        selected.push(SelectedQueryLine {
            end_offset: line.end_offset,
            ordinal: line.ordinal,
            value: std::mem::take(&mut transformed),
        });
    }
    let next_cursor = if truncated {
        selected
            .last()
            .map(|line| {
                FileCursor::new(
                    "query_rows",
                    invocation_id,
                    None,
                    line.end_offset,
                    line.ordinal,
                )
                .encode()
            })
            .transpose()?
    } else {
        None
    };
    Ok((
        Page {
            items: selected
                .into_iter()
                .map(|line| QueryRow {
                    ordinal: line.ordinal,
                    value: line.value,
                })
                .collect(),
            next_cursor,
            truncated,
        },
        total,
        total,
    ))
}

#[derive(Serialize)]
struct BorrowedQueryRow<'a> {
    ordinal: u64,
    value: &'a str,
}

fn serialized_query_row_len(
    ordinal: u64,
    value: &str,
    buffer: &mut Vec<u8>,
) -> Result<usize, StoreError> {
    buffer.clear();
    serde_json::to_writer(&mut *buffer, &BorrowedQueryRow { ordinal, value })?;
    Ok(buffer.len())
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
    pub(crate) limit: usize,
    value: Vec<u8>,
}

struct BoundedLine<'a> {
    pub(crate) start_offset: u64,
    end_offset: u64,
    ordinal: u64,
    value: Cow<'a, str>,
}

struct SelectedQueryLine {
    end_offset: u64,
    ordinal: u64,
    value: String,
}

impl<R: BufRead> BoundedLineReader<R> {
    fn new(reader: R, limit: usize) -> Self {
        Self::with_position(reader, limit, 0, 0)
    }

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
        let start_offset = self.offset;
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
            start_offset,
            end_offset: self.offset,
            ordinal,
            value: String::from_utf8_lossy(&self.value),
        }))
    }
}
