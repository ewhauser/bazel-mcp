//! Durable manifest schema and compatibility decoding.

use std::path::Path;

#[cfg(test)]
use std::path::PathBuf;

use bazel_mcp_types::DeferredResultRecord;
use serde::{Deserialize, Serialize};

use crate::{record::InvocationHeader, storage::StoreError};

pub(crate) const CURRENT_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct DurableRecord {
    pub(crate) schema_version: u32,
    pub(crate) invocation: InvocationHeader,
    #[serde(default)]
    pub(crate) deferred: Option<DeferredResultRecord>,
    #[serde(default)]
    pub(crate) payload_bytes: u64,
}

pub(crate) async fn read(path: &Path) -> Result<(DurableRecord, u64), StoreError> {
    let bytes = tokio::fs::read(path).await?;
    let manifest_bytes = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    Ok((decode(path, &bytes)?, manifest_bytes))
}

pub(crate) fn decode(path: &Path, bytes: &[u8]) -> Result<DurableRecord, StoreError> {
    #[derive(Deserialize)]
    struct SchemaEnvelope {
        schema_version: u32,
    }

    let durable = match serde_json::from_slice::<DurableRecord>(bytes) {
        Ok(durable) => durable,
        Err(error) => {
            // If a future manifest no longer resembles v1, still report its
            // version accurately instead of misclassifying it as corrupt.
            if let Ok(envelope) = serde_json::from_slice::<SchemaEnvelope>(bytes)
                && envelope.schema_version != CURRENT_SCHEMA_VERSION
            {
                return Err(StoreError::UnsupportedRecordSchema {
                    found: envelope.schema_version,
                    path: path.to_owned(),
                });
            }
            return Err(StoreError::CorruptRecord {
                path: path.to_owned(),
                message: error.to_string(),
            });
        }
    };
    match durable.schema_version {
        // New schema versions get an explicit migration arm here. Keeping the
        // dispatch centralized prevents opportunistic serde defaults from
        // silently changing durable semantics.
        1 => Ok(migrate_v1(durable)),
        found => Err(StoreError::UnsupportedRecordSchema {
            found,
            path: path.to_owned(),
        }),
    }
}

fn migrate_v1(durable: DurableRecord) -> DurableRecord {
    durable
}

#[cfg(test)]
mod tests {
    use super::*;

    const V1_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/manifest-v1.json");

    #[test]
    fn schema_v1_fixture_round_trips_without_shape_drift() {
        let path = PathBuf::from("manifest-v1.json");
        let decoded = decode(&path, V1_FIXTURE).unwrap();
        assert_eq!(decoded.schema_version, 1);
        let expected: serde_json::Value = serde_json::from_slice(V1_FIXTURE).unwrap();
        let actual = serde_json::to_value(decoded).unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn future_schema_requires_an_explicit_migration() {
        let path = PathBuf::from("future.json");
        let mut value: serde_json::Value = serde_json::from_slice(V1_FIXTURE).unwrap();
        value["schema_version"] = serde_json::json!(2);
        let error = decode(&path, &serde_json::to_vec(&value).unwrap()).unwrap_err();
        assert!(matches!(
            error,
            StoreError::UnsupportedRecordSchema { found: 2, .. }
        ));

        let sparse_future = br#"{"schema_version":3,"replacement":"shape"}"#;
        let error = decode(&path, sparse_future).unwrap_err();
        assert!(matches!(
            error,
            StoreError::UnsupportedRecordSchema { found: 3, .. }
        ));
    }
}
