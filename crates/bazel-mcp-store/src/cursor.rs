use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::StoreError;

const VERSION: u8 = 1;
const INVOCATION_KIND: u8 = 1;
const DEFERRED_KIND: u8 = 2;
const ORDINAL_KIND: u8 = 3;
const FILE_KIND: u8 = 4;
const CONTEXT_BYTES: usize = 16;

#[derive(Debug)]
pub(crate) struct InvocationCursor {
    context: [u8; CONTEXT_BYTES],
    pub requested_at_ms: i64,
    pub id: String,
}

#[derive(Debug)]
pub(crate) struct DeferredCursor {
    context: [u8; CONTEXT_BYTES],
    pub created_at_ms: i64,
    pub id: String,
}

#[derive(Debug)]
pub(crate) struct OrdinalCursor {
    context: [u8; CONTEXT_BYTES],
    pub ordinal: i64,
}

#[derive(Debug)]
pub(crate) struct FileCursor {
    context: [u8; CONTEXT_BYTES],
    pub offset: u64,
    pub ordinal: u64,
    pub total_scanned: u64,
    pub filtered_scanned: u64,
}

impl DeferredCursor {
    pub fn new(retrieval: &str, created_at_ms: i64, id: String) -> Self {
        Self {
            context: cursor_context(&["deferred", retrieval]),
            created_at_ms,
            id,
        }
    }

    pub fn encode(&self) -> Result<String, StoreError> {
        encode_timestamp_cursor(DEFERRED_KIND, self.context, self.created_at_ms, &self.id)
    }

    pub fn decode(value: &str) -> Result<Self, StoreError> {
        let (context, created_at_ms, id) = decode_timestamp_cursor(value, DEFERRED_KIND)?;
        Ok(Self {
            context,
            created_at_ms,
            id,
        })
    }

    pub fn decode_for(value: &str, retrieval: &str) -> Result<Self, StoreError> {
        let cursor = Self::decode(value)?;
        if cursor.context != cursor_context(&["deferred", retrieval]) {
            return Err(StoreError::InvalidCursor);
        }
        Ok(cursor)
    }
}

impl InvocationCursor {
    pub fn new(
        workspace: Option<&str>,
        state: Option<&str>,
        command: Option<&str>,
        requested_at_ms: i64,
        id: String,
    ) -> Self {
        Self {
            context: cursor_context(&[
                "invocations",
                workspace.unwrap_or_default(),
                state.unwrap_or_default(),
                command.unwrap_or_default(),
            ]),
            requested_at_ms,
            id,
        }
    }

    pub fn encode(&self) -> Result<String, StoreError> {
        encode_timestamp_cursor(
            INVOCATION_KIND,
            self.context,
            self.requested_at_ms,
            &self.id,
        )
    }

    pub fn decode(value: &str) -> Result<Self, StoreError> {
        let (context, requested_at_ms, id) = decode_timestamp_cursor(value, INVOCATION_KIND)?;
        Ok(Self {
            context,
            requested_at_ms,
            id,
        })
    }

    pub fn decode_for(
        value: &str,
        workspace: Option<&str>,
        state: Option<&str>,
        command: Option<&str>,
    ) -> Result<Self, StoreError> {
        let cursor = Self::decode(value)?;
        if cursor.context
            != cursor_context(&[
                "invocations",
                workspace.unwrap_or_default(),
                state.unwrap_or_default(),
                command.unwrap_or_default(),
            ])
        {
            return Err(StoreError::InvalidCursor);
        }
        Ok(cursor)
    }
}

impl FileCursor {
    pub fn new(
        scope: &str,
        invocation_id: &str,
        filter: Option<&str>,
        offset: u64,
        ordinal: u64,
        total_scanned: u64,
        filtered_scanned: u64,
    ) -> Self {
        Self {
            context: cursor_context(&[scope, invocation_id, filter.unwrap_or_default()]),
            offset,
            ordinal,
            total_scanned,
            filtered_scanned,
        }
    }

    pub fn encode(&self) -> Result<String, StoreError> {
        let mut bytes = Vec::with_capacity(50);
        bytes.extend_from_slice(&[VERSION, FILE_KIND]);
        bytes.extend_from_slice(&self.context);
        bytes.extend_from_slice(&self.offset.to_le_bytes());
        bytes.extend_from_slice(&self.ordinal.to_le_bytes());
        bytes.extend_from_slice(&self.total_scanned.to_le_bytes());
        bytes.extend_from_slice(&self.filtered_scanned.to_le_bytes());
        Ok(URL_SAFE_NO_PAD.encode(bytes))
    }

    pub fn decode(value: &str) -> Result<Self, StoreError> {
        let bytes = decode_exact(value, 50, FILE_KIND)?;
        let context = bytes[2..18]
            .try_into()
            .map_err(|_| StoreError::InvalidCursor)?;
        let offset = u64::from_le_bytes(
            bytes[18..26]
                .try_into()
                .map_err(|_| StoreError::InvalidCursor)?,
        );
        let ordinal = u64::from_le_bytes(
            bytes[26..34]
                .try_into()
                .map_err(|_| StoreError::InvalidCursor)?,
        );
        let total_scanned = u64::from_le_bytes(
            bytes[34..42]
                .try_into()
                .map_err(|_| StoreError::InvalidCursor)?,
        );
        let filtered_scanned = u64::from_le_bytes(
            bytes[42..50]
                .try_into()
                .map_err(|_| StoreError::InvalidCursor)?,
        );
        Ok(Self {
            context,
            offset,
            ordinal,
            total_scanned,
            filtered_scanned,
        })
    }

    pub fn decode_for(
        value: &str,
        scope: &str,
        invocation_id: &str,
        filter: Option<&str>,
    ) -> Result<Self, StoreError> {
        let cursor = Self::decode(value)?;
        if cursor.context != cursor_context(&[scope, invocation_id, filter.unwrap_or_default()]) {
            return Err(StoreError::InvalidCursor);
        }
        Ok(cursor)
    }
}

impl OrdinalCursor {
    pub fn new(scope: &str, invocation_id: &str, filter: Option<&str>, ordinal: i64) -> Self {
        Self {
            context: cursor_context(&[scope, invocation_id, filter.unwrap_or_default()]),
            ordinal,
        }
    }

    pub fn encode(&self) -> Result<String, StoreError> {
        let mut bytes = Vec::with_capacity(26);
        bytes.extend_from_slice(&[VERSION, ORDINAL_KIND]);
        bytes.extend_from_slice(&self.context);
        bytes.extend_from_slice(&self.ordinal.to_le_bytes());
        Ok(URL_SAFE_NO_PAD.encode(bytes))
    }

    pub fn decode(value: &str) -> Result<Self, StoreError> {
        let bytes = decode_exact(value, 26, ORDINAL_KIND)?;
        let context = bytes[2..18]
            .try_into()
            .map_err(|_| StoreError::InvalidCursor)?;
        let ordinal = i64::from_le_bytes(
            bytes[18..26]
                .try_into()
                .map_err(|_| StoreError::InvalidCursor)?,
        );
        Ok(Self { context, ordinal })
    }

    pub fn decode_for(
        value: &str,
        scope: &str,
        invocation_id: &str,
        filter: Option<&str>,
    ) -> Result<Self, StoreError> {
        let cursor = Self::decode(value)?;
        if cursor.context != cursor_context(&[scope, invocation_id, filter.unwrap_or_default()]) {
            return Err(StoreError::InvalidCursor);
        }
        Ok(cursor)
    }
}

fn encode_timestamp_cursor(
    kind: u8,
    context: [u8; CONTEXT_BYTES],
    timestamp: i64,
    id: &str,
) -> Result<String, StoreError> {
    let uuid = Uuid::parse_str(id).map_err(|_| StoreError::InvalidCursor)?;
    let mut bytes = Vec::with_capacity(42);
    bytes.extend_from_slice(&[VERSION, kind]);
    bytes.extend_from_slice(&context);
    bytes.extend_from_slice(&timestamp.to_le_bytes());
    bytes.extend_from_slice(uuid.as_bytes());
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn decode_timestamp_cursor(
    value: &str,
    kind: u8,
) -> Result<([u8; CONTEXT_BYTES], i64, String), StoreError> {
    let bytes = decode_exact(value, 42, kind)?;
    let context = bytes[2..18]
        .try_into()
        .map_err(|_| StoreError::InvalidCursor)?;
    let timestamp = i64::from_le_bytes(
        bytes[18..26]
            .try_into()
            .map_err(|_| StoreError::InvalidCursor)?,
    );
    let uuid = Uuid::from_slice(&bytes[26..42]).map_err(|_| StoreError::InvalidCursor)?;
    Ok((context, timestamp, uuid.to_string()))
}

fn decode_exact(value: &str, expected_len: usize, kind: u8) -> Result<Vec<u8>, StoreError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| StoreError::InvalidCursor)?;
    if bytes.len() != expected_len || bytes[0] != VERSION || bytes[1] != kind {
        return Err(StoreError::InvalidCursor);
    }
    Ok(bytes)
}

fn cursor_context(parts: &[&str]) -> [u8; CONTEXT_BYTES] {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(u64::try_from(part.len()).unwrap_or(u64::MAX).to_le_bytes());
        hasher.update(part.as_bytes());
    }
    let digest = hasher.finalize();
    let mut context = [0_u8; CONTEXT_BYTES];
    context.copy_from_slice(&digest[..CONTEXT_BYTES]);
    context
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_binary_cursors_are_compact_and_context_bound() {
        let id = Uuid::now_v7().to_string();
        let file = FileCursor::new("query_rows", &id, Some("needle"), 42, 7, 8, 3);
        let encoded = file.encode().unwrap();
        assert_eq!(encoded.len(), 67);
        let decoded = FileCursor::decode_for(&encoded, "query_rows", &id, Some("needle")).unwrap();
        assert_eq!(decoded.offset, 42);
        assert_eq!(decoded.ordinal, 7);
        assert_eq!(decoded.total_scanned, 8);
        assert_eq!(decoded.filtered_scanned, 3);
        assert!(FileCursor::decode_for(&encoded, "query_rows", &id, Some("other")).is_err());
        assert!(
            FileCursor::decode_for(
                &encoded,
                "query_rows",
                &Uuid::now_v7().to_string(),
                Some("needle")
            )
            .is_err()
        );
        assert!(FileCursor::decode_for(&encoded, "log", &id, Some("needle")).is_err());
        assert!(OrdinalCursor::decode(&encoded).is_err());
    }

    #[test]
    fn cursors_reject_versions_lengths_and_cross_context_use() {
        let id = Uuid::now_v7().to_string();
        let cursor =
            InvocationCursor::new(Some("/workspace"), Some("failed"), Some("test"), 123, id);
        let encoded = cursor.encode().unwrap();
        assert!(
            InvocationCursor::decode_for(
                &encoded,
                Some("/workspace"),
                Some("failed"),
                Some("test")
            )
            .is_ok()
        );
        assert!(
            InvocationCursor::decode_for(&encoded, Some("/other"), Some("failed"), Some("test"))
                .is_err()
        );
        assert!(
            InvocationCursor::decode_for(
                &encoded,
                Some("/workspace"),
                Some("succeeded"),
                Some("test")
            )
            .is_err()
        );
        assert!(
            InvocationCursor::decode_for(
                &encoded,
                Some("/workspace"),
                Some("failed"),
                Some("build")
            )
            .is_err()
        );

        let mut bytes = URL_SAFE_NO_PAD.decode(&encoded).unwrap();
        bytes[0] = VERSION + 1;
        assert!(InvocationCursor::decode(&URL_SAFE_NO_PAD.encode(bytes)).is_err());
        assert!(InvocationCursor::decode(&encoded[..encoded.len() - 1]).is_err());
    }
}
