use std::{num::NonZeroU64, time::Duration};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Serializable evidence-retention settings owned by the store.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RawRetentionConfig {
    retention_days: u64,
    maximum_storage_bytes: u64,
    retention_cleanup_interval_seconds: u64,
}

impl Default for RawRetentionConfig {
    fn default() -> Self {
        Self {
            retention_days: 7,
            maximum_storage_bytes: 10 * 1024 * 1024 * 1024,
            retention_cleanup_interval_seconds: 60 * 60,
        }
    }
}

/// Validated retention policy consumed by store maintenance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetentionConfig {
    pub maximum_age: Duration,
    pub maximum_storage_bytes: NonZeroU64,
    pub cleanup_interval: Duration,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum RetentionConfigError {
    #[error("maximum storage bytes must be greater than zero")]
    ZeroMaximumStorageBytes,
    #[error("retention cleanup interval must be greater than zero")]
    ZeroCleanupInterval,
}

impl TryFrom<RawRetentionConfig> for RetentionConfig {
    type Error = RetentionConfigError;

    fn try_from(raw: RawRetentionConfig) -> Result<Self, Self::Error> {
        let maximum_storage_bytes = NonZeroU64::new(raw.maximum_storage_bytes)
            .ok_or(RetentionConfigError::ZeroMaximumStorageBytes)?;
        if raw.retention_cleanup_interval_seconds == 0 {
            return Err(RetentionConfigError::ZeroCleanupInterval);
        }
        Ok(Self {
            maximum_age: Duration::from_secs(raw.retention_days.saturating_mul(24 * 60 * 60)),
            maximum_storage_bytes,
            cleanup_interval: Duration::from_secs(raw.retention_cleanup_interval_seconds),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid_and_owned_by_the_store() {
        let config = RetentionConfig::try_from(RawRetentionConfig::default()).unwrap();
        assert_eq!(config.maximum_age, Duration::from_secs(7 * 24 * 60 * 60));
        assert_eq!(config.maximum_storage_bytes.get(), 10 * 1024 * 1024 * 1024);
        assert_eq!(config.cleanup_interval, Duration::from_secs(60 * 60));
    }

    #[test]
    fn rejects_zero_quota_and_cleanup_interval() {
        let zero_bytes = RawRetentionConfig {
            maximum_storage_bytes: 0,
            ..RawRetentionConfig::default()
        };
        assert_eq!(
            RetentionConfig::try_from(zero_bytes),
            Err(RetentionConfigError::ZeroMaximumStorageBytes)
        );

        let zero_interval = RawRetentionConfig {
            retention_cleanup_interval_seconds: 0,
            ..RawRetentionConfig::default()
        };
        assert_eq!(
            RetentionConfig::try_from(zero_interval),
            Err(RetentionConfigError::ZeroCleanupInterval)
        );
    }
}
