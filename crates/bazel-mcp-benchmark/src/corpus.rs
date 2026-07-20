use std::path::Path;

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProjectManifest {
    pub(crate) name: String,
    url: String,
    release_tag: String,
    pub(crate) commit: String,
    license: String,
    pub(crate) bazel_version: String,
    pub scenarios: Vec<Scenario>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Scenario {
    pub(crate) name: String,
    pub(crate) command: String,
    pub(crate) args: Vec<String>,
    pub(crate) expected_exit: i32,
    pub(crate) expected_cause: Option<String>,
    #[serde(default)]
    cache_condition: String,
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
