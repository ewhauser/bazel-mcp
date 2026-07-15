use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::StoreError;

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct InvocationCursor {
    context: String,
    pub requested_at_ms: i64,
    pub id: String,
}

impl InvocationCursor {
    pub fn new(workspace: Option<&str>, requested_at_ms: i64, id: String) -> Self {
        Self {
            context: cursor_context(&["invocations", workspace.unwrap_or_default()]),
            requested_at_ms,
            id,
        }
    }

    pub fn encode(&self) -> Result<String, StoreError> {
        Ok(URL_SAFE_NO_PAD.encode(serde_json::to_vec(self)?))
    }

    pub fn decode(value: &str) -> Result<Self, StoreError> {
        let raw = URL_SAFE_NO_PAD
            .decode(value)
            .map_err(|_| StoreError::InvalidCursor)?;
        serde_json::from_slice(&raw).map_err(|_| StoreError::InvalidCursor)
    }

    pub fn decode_for(value: &str, workspace: Option<&str>) -> Result<Self, StoreError> {
        let cursor = Self::decode(value)?;
        let expected = cursor_context(&["invocations", workspace.unwrap_or_default()]);
        if cursor.context != expected {
            return Err(StoreError::InvalidCursor);
        }
        Ok(cursor)
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct OrdinalCursor {
    context: String,
    pub ordinal: i64,
}

impl OrdinalCursor {
    pub fn new(scope: &str, invocation_id: &str, filter: Option<&str>, ordinal: i64) -> Self {
        Self {
            context: cursor_context(&[scope, invocation_id, filter.unwrap_or_default()]),
            ordinal,
        }
    }

    pub fn encode(&self) -> Result<String, StoreError> {
        Ok(URL_SAFE_NO_PAD.encode(serde_json::to_vec(self)?))
    }

    pub fn decode(value: &str) -> Result<Self, StoreError> {
        let raw = URL_SAFE_NO_PAD
            .decode(value)
            .map_err(|_| StoreError::InvalidCursor)?;
        serde_json::from_slice(&raw).map_err(|_| StoreError::InvalidCursor)
    }

    pub fn decode_for(
        value: &str,
        scope: &str,
        invocation_id: &str,
        filter: Option<&str>,
    ) -> Result<Self, StoreError> {
        let cursor = Self::decode(value)?;
        let expected = cursor_context(&[scope, invocation_id, filter.unwrap_or_default()]);
        if cursor.context != expected {
            return Err(StoreError::InvalidCursor);
        }
        Ok(cursor)
    }
}

fn cursor_context(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(u64::try_from(part.len()).unwrap_or(u64::MAX).to_le_bytes());
        hasher.update(part.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}
