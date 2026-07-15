use std::{collections::BTreeSet, path::PathBuf};

use bazel_mcp_types::BazelCommand;
use serde::{Deserialize, Serialize};

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
    #[must_use]
    pub fn permits(&self, command: &BazelCommand) -> bool {
        let name = command.as_str();
        self.allowed_commands.contains(name) && !self.denied_commands.contains(name)
    }
}
