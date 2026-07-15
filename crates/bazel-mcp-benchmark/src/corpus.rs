use std::path::Path;

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProjectManifest {
    pub name: String,
    pub url: String,
    pub release_tag: String,
    pub commit: String,
    pub license: String,
    pub bazel_version: String,
    pub scenarios: Vec<Scenario>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Scenario {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub expected_exit: i32,
    pub expected_cause: Option<String>,
    #[serde(default)]
    pub cache_condition: String,
}

impl ProjectManifest {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let source = std::fs::read_to_string(path)
            .with_context(|| format!("read corpus manifest {}", path.display()))?;
        let manifest: Self = toml::from_str(&source)
            .with_context(|| format!("parse corpus manifest {}", path.display()))?;
        if manifest.commit.len() != 40
            || !manifest.commit.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            anyhow::bail!("corpus commit must be a full 40-character SHA-1");
        }
        if manifest.scenarios.is_empty() {
            anyhow::bail!("corpus manifest has no scenarios");
        }
        Ok(manifest)
    }
}
