use std::{collections::BTreeSet, path::PathBuf};

use bazel_mcp_types::{BazelCommand, CommandClass};
use serde::{Deserialize, Serialize};

/// Serializable Aspect CLI settings owned by the runner.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RawAspectConfig {
    pub executable: Option<PathBuf>,
    pub commands: BTreeSet<String>,
    pub allow_workspace_mutation: bool,
}

/// Optional Aspect CLI command routing.
///
/// Commands not listed here continue to use the direct Bazel driver. Keeping
/// routing separate from `allowed_commands` preserves policy as an independent
/// authorization decision.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AspectConfig {
    pub executable: Option<PathBuf>,
    pub commands: BTreeSet<String>,
    pub allow_workspace_mutation: bool,
}

impl From<RawAspectConfig> for AspectConfig {
    fn from(raw: RawAspectConfig) -> Self {
        Self {
            executable: raw.executable,
            commands: raw.commands,
            allow_workspace_mutation: raw.allow_workspace_mutation,
        }
    }
}

impl From<AspectConfig> for RawAspectConfig {
    fn from(config: AspectConfig) -> Self {
        Self {
            executable: config.executable,
            commands: config.commands,
            allow_workspace_mutation: config.allow_workspace_mutation,
        }
    }
}

impl AspectConfig {
    #[must_use]
    pub fn enabled(&self) -> bool {
        !self.commands.is_empty()
    }

    #[must_use]
    pub fn routes(&self, command: &BazelCommand) -> bool {
        self.commands.contains(command.as_str())
    }

    #[must_use]
    pub(crate) fn has_invalid_command(&self) -> bool {
        self.commands.iter().any(|command| {
            command.is_empty()
                || command.starts_with('-')
                || command.chars().any(char::is_whitespace)
        })
    }
}

#[derive(Clone, Debug)]
pub(crate) enum ExecutionDriver {
    Bazel {
        executable: PathBuf,
    },
    Aspect {
        executable: PathBuf,
        bazel_executable: PathBuf,
    },
}

impl ExecutionDriver {
    #[must_use]
    pub(crate) const fn is_aspect(&self) -> bool {
        matches!(self, Self::Aspect { .. })
    }

    #[must_use]
    pub(crate) fn executable(&self) -> &std::path::Path {
        match self {
            Self::Bazel { executable } | Self::Aspect { executable, .. } => executable,
        }
    }

    #[must_use]
    pub(crate) fn command_class(&self, command: &BazelCommand) -> CommandClass {
        if self.is_aspect() && matches!(command.as_str(), "build" | "test" | "lint") {
            CommandClass::BuildLike
        } else {
            command.class()
        }
    }

    #[must_use]
    pub(crate) fn bazel_executable(&self) -> &std::path::Path {
        match self {
            Self::Bazel { executable }
            | Self::Aspect {
                bazel_executable: executable,
                ..
            } => executable,
        }
    }

    #[must_use]
    pub(crate) const fn display_name(&self) -> &'static str {
        match self {
            Self::Bazel { .. } => "Bazel",
            Self::Aspect { .. } => "Aspect CLI",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aspect_lint_is_build_like_without_reclassifying_direct_custom_commands() {
        let lint = BazelCommand::Custom("lint".to_owned());
        let direct = ExecutionDriver::Bazel {
            executable: "bazel".into(),
        };
        let aspect = ExecutionDriver::Aspect {
            executable: "aspect".into(),
            bazel_executable: "bazel".into(),
        };
        assert_eq!(direct.command_class(&lint), CommandClass::Unknown);
        assert_eq!(aspect.command_class(&lint), CommandClass::BuildLike);
    }

    #[test]
    fn aspect_routes_must_be_command_names_not_wrapper_arguments() {
        let mut config = AspectConfig::default();
        config.commands.insert("--help".to_owned());
        assert!(config.has_invalid_command());
        config.commands = ["lint".to_owned()].into();
        assert!(!config.has_invalid_command());
    }
}
