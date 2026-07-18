use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandClass {
    BuildLike,
    Run,
    Query,
    Informational,
    Unsafe,
    Unknown,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BazelCommand {
    Build,
    Test,
    Coverage,
    Query,
    Cquery,
    Aquery,
    Info,
    Version,
    Help,
    Mod,
    MobileInstall,
    Clean,
    Shutdown,
    Run,
    Custom(String),
}

impl BazelCommand {
    #[must_use]
    pub const fn class(&self) -> CommandClass {
        match self {
            Self::Build | Self::Test | Self::Coverage | Self::MobileInstall => {
                CommandClass::BuildLike
            }
            Self::Run => CommandClass::Run,
            Self::Query | Self::Cquery | Self::Aquery => CommandClass::Query,
            Self::Info | Self::Version | Self::Help | Self::Mod => CommandClass::Informational,
            Self::Clean | Self::Shutdown => CommandClass::Unsafe,
            Self::Custom(_) => CommandClass::Unknown,
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Build => "build",
            Self::Test => "test",
            Self::Coverage => "coverage",
            Self::Query => "query",
            Self::Cquery => "cquery",
            Self::Aquery => "aquery",
            Self::Info => "info",
            Self::Version => "version",
            Self::Help => "help",
            Self::Mod => "mod",
            Self::MobileInstall => "mobile-install",
            Self::Clean => "clean",
            Self::Shutdown => "shutdown",
            Self::Run => "run",
            Self::Custom(value) => value,
        }
    }
}

impl fmt::Display for BazelCommand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for BazelCommand {
    type Err = std::convert::Infallible;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(match value {
            "build" => Self::Build,
            "test" => Self::Test,
            "coverage" => Self::Coverage,
            "query" => Self::Query,
            "cquery" => Self::Cquery,
            "aquery" => Self::Aquery,
            "info" => Self::Info,
            "version" => Self::Version,
            "help" => Self::Help,
            "mod" => Self::Mod,
            "mobile-install" => Self::MobileInstall,
            "clean" => Self::Clean,
            "shutdown" => Self::Shutdown,
            "run" => Self::Run,
            other => Self::Custom(other.to_owned()),
        })
    }
}
