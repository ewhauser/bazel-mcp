use std::{collections::BTreeSet, path::PathBuf};

use serde::{Deserialize, Serialize};

use crate::{PolicyError, redaction::Redactor};

/// Serializable policy settings owned and validated by the policy subsystem.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RawPolicyConfig {
    pub allowed_roots: Vec<PathBuf>,
    allowed_commands: BTreeSet<String>,
    denied_commands: BTreeSet<String>,
    environment_allowlist: BTreeSet<String>,
    redaction_patterns: Vec<String>,
    pub bazel_executable: Option<PathBuf>,
}

impl Default for RawPolicyConfig {
    fn default() -> Self {
        let config = PolicyConfig::default();
        Self {
            allowed_roots: config.allowed_roots,
            allowed_commands: config.allowed_commands,
            denied_commands: config.denied_commands,
            environment_allowlist: config.environment_allowlist,
            redaction_patterns: config.redaction_patterns,
            bazel_executable: config.bazel_executable,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PolicyConfig {
    pub allowed_roots: Vec<PathBuf>,
    #[serde(default = "default_allowed_commands")]
    pub allowed_commands: BTreeSet<String>,
    #[serde(default = "default_denied_commands")]
    pub denied_commands: BTreeSet<String>,
    #[serde(default)]
    pub environment_allowlist: BTreeSet<String>,
    #[serde(default)]
    pub redaction_patterns: Vec<String>,
    pub bazel_executable: Option<PathBuf>,
}

fn default_allowed_commands() -> BTreeSet<String> {
    [
        "aquery", "build", "coverage", "cquery", "help", "info", "mod", "query", "test", "version",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn default_denied_commands() -> BTreeSet<String> {
    [
        "clean",
        "fetch",
        "mobile-install",
        "run",
        "shutdown",
        "sync",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            allowed_roots: Vec::new(),
            allowed_commands: default_allowed_commands(),
            denied_commands: default_denied_commands(),
            environment_allowlist: BTreeSet::new(),
            redaction_patterns: Vec::new(),
            bazel_executable: None,
        }
    }
}

impl PolicyConfig {
    /// Validate policy-owned configuration before it reaches request handling.
    fn validate(&self) -> Result<(), PolicyError> {
        Redactor::new(&self.redaction_patterns)?;
        Ok(())
    }
}

impl TryFrom<RawPolicyConfig> for PolicyConfig {
    type Error = PolicyError;

    fn try_from(raw: RawPolicyConfig) -> Result<Self, Self::Error> {
        let config = Self {
            allowed_roots: raw.allowed_roots,
            allowed_commands: raw.allowed_commands,
            denied_commands: raw.denied_commands,
            environment_allowlist: raw.environment_allowlist,
            redaction_patterns: raw.redaction_patterns,
            bazel_executable: raw.bazel_executable,
        };
        config.validate()?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_defaults_project_to_runtime_defaults() {
        let expected = PolicyConfig::default();
        let actual = PolicyConfig::try_from(RawPolicyConfig::default()).unwrap();
        assert_eq!(actual.allowed_roots, expected.allowed_roots);
        assert_eq!(actual.allowed_commands, expected.allowed_commands);
        assert_eq!(actual.denied_commands, expected.denied_commands);
        assert_eq!(actual.environment_allowlist, expected.environment_allowlist);
        assert_eq!(actual.redaction_patterns, expected.redaction_patterns);
        assert_eq!(actual.bazel_executable, expected.bazel_executable);
    }

    #[test]
    fn raw_policy_rejects_invalid_redaction_patterns() {
        let raw = RawPolicyConfig {
            redaction_patterns: vec!["[".to_owned()],
            ..RawPolicyConfig::default()
        };
        assert!(matches!(
            PolicyConfig::try_from(raw),
            Err(PolicyError::InvalidRedaction { .. })
        ));
    }
}
