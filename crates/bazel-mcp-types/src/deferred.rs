use serde::{Deserialize, Serialize};

use crate::{InvocationId, InvocationRecord};

/// Whether a submitted invocation stays attached to its caller or is durable.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultDisposition {
    Attached,
    Deferred {
        retrieval: DeferredRetrieval,
        expires_at_ms: i64,
    },
}

/// Protocol-neutral result placement for a deferred invocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeferredRetrieval {
    SeparateResult,
    InlineResult,
}

impl DeferredRetrieval {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SeparateResult => "separate_result",
            Self::InlineResult => "inline_result",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "separate_result" => Some(Self::SeparateResult),
            "inline_result" => Some(Self::InlineResult),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeferredTerminalState {
    Cancelled,
}

impl DeferredTerminalState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeferredFailureKind {
    Queue,
    Execution,
    Internal,
}

impl DeferredFailureKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queue => "queue",
            Self::Execution => "execution",
            Self::Internal => "internal",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "queue" => Some(Self::Queue),
            "execution" => Some(Self::Execution),
            "internal" => Some(Self::Internal),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DeferredFailure {
    pub kind: DeferredFailureKind,
    pub redacted_message: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DeferredResultRecord {
    pub invocation_id: InvocationId,
    pub retrieval: DeferredRetrieval,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub expires_at_ms: i64,
    pub cancellation_requested_at_ms: Option<i64>,
    pub terminal_override: Option<DeferredTerminalState>,
    pub failure: Option<DeferredFailure>,
}

impl DeferredResultRecord {
    #[must_use]
    pub fn new(
        invocation_id: InvocationId,
        retrieval: DeferredRetrieval,
        created_at_ms: i64,
        expires_at_ms: i64,
    ) -> Self {
        Self {
            invocation_id,
            retrieval,
            created_at_ms,
            updated_at_ms: created_at_ms,
            expires_at_ms: expires_at_ms.max(created_at_ms.saturating_add(1)),
            cancellation_requested_at_ms: None,
            terminal_override: None,
            failure: None,
        }
    }

    #[must_use]
    pub fn configured_ttl_ms(&self) -> i64 {
        self.expires_at_ms.saturating_sub(self.updated_at_ms).max(1)
    }

    /// Extend expiry so a terminal result remains available for one configured TTL.
    pub fn extend_terminal_expiry(&mut self, terminal_at_ms: i64) -> bool {
        let required = terminal_at_ms.saturating_add(self.configured_ttl_ms());
        if required > self.expires_at_ms {
            self.expires_at_ms = required;
            self.updated_at_ms = terminal_at_ms.max(self.updated_at_ms);
            true
        } else {
            false
        }
    }

    #[must_use]
    pub fn advertised_ttl_ms(&self) -> u64 {
        u64::try_from(self.expires_at_ms.saturating_sub(self.created_at_ms)).unwrap_or(u64::MAX)
    }

    #[must_use]
    pub fn is_expired(&self, now_ms: i64, invocation_terminal: bool) -> bool {
        invocation_terminal && self.expires_at_ms <= now_ms
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeferredResultView {
    pub deferred: DeferredResultRecord,
    pub invocation: InvocationRecord,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_expiry_is_deterministic_and_never_shortens() {
        let mut record = DeferredResultRecord::new(
            InvocationId::new(),
            DeferredRetrieval::InlineResult,
            1_000,
            11_000,
        );
        assert_eq!(record.configured_ttl_ms(), 10_000);
        assert!(record.extend_terminal_expiry(20_000));
        assert_eq!(record.expires_at_ms, 30_000);
        assert_eq!(record.advertised_ttl_ms(), 29_000);
        assert!(!record.extend_terminal_expiry(15_000));
        assert_eq!(record.expires_at_ms, 30_000);
    }

    #[test]
    fn nonterminal_tasks_do_not_expire() {
        let record = DeferredResultRecord::new(
            InvocationId::new(),
            DeferredRetrieval::SeparateResult,
            1_000,
            2_000,
        );
        assert!(!record.is_expired(3_000, false));
        assert!(record.is_expired(3_000, true));
    }
}
