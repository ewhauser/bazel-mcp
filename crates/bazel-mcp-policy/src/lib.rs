//! Workspace, command, environment, executable, and redaction policy.

mod command;
mod config;
mod environment;
mod executable;
mod flags;
mod redaction;
mod workspace;

pub use command::validate_command;
pub use config::{PolicyConfig, RawPolicyConfig};
pub use environment::filtered_environment;
pub use executable::{
    resolve_aspect_executable, resolve_bazel_executable, resolve_bazel_executable_excluding,
};
pub use flags::{
    INTERNAL_BEP_FLAG, effective_output_base, validate_arguments, validate_aspect_arguments,
    validate_query_arguments, validate_run_arguments,
};
pub use redaction::Redactor;
pub use workspace::validate_workspace;

use std::{io, path::PathBuf};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum PolicyError {
    #[error("workspace path must be absolute: {0}")]
    WorkspaceNotAbsolute(PathBuf),
    #[error("workspace is outside every configured allowed root: {0}")]
    WorkspaceNotAllowed(PathBuf),
    #[error("directory is not a Bazel workspace: {0}")]
    NotBazelWorkspace(PathBuf),
    #[error("Bazel command is denied by policy: {0}")]
    CommandDenied(String),
    #[error("unsupported Bazel command: {0}")]
    CommandUnsupported(String),
    #[error("argument overrides a server-owned flag: {0}")]
    ReservedFlag(String),
    #[error("argument contains a NUL byte")]
    InvalidArgument,
    #[error("bazel run requires exactly one non-empty target")]
    RunTargetRequired,
    #[error("bazel run target must not look like a command option")]
    InvalidRunTarget,
    #[error("bazel run command arguments must be flags; use --flag=value for valued options")]
    RunArgumentsMustBeFlags,
    #[error("target and program_args are only valid when command is run")]
    RunFieldsOnNonRunCommand,
    #[error("bazel run is currently supported only on Unix platforms")]
    RunUnsupportedPlatform,
    #[error("argument exceeds the {maximum_bytes}-byte limit")]
    ArgumentTooLong { maximum_bytes: usize },
    #[error("argument list exceeds the {maximum_count}-item limit")]
    TooManyArguments { maximum_count: usize },
    #[error("argument list exceeds the {maximum_bytes}-byte aggregate limit")]
    ArgumentsTooLarge { maximum_bytes: usize },
    #[error("query output format is not supported by the streaming text adapter: {0}")]
    IncompatibleQueryOutput(String),
    #[error("--output_base requires a path value")]
    MissingOutputBaseValue,
    #[error("--output_base may be specified only once")]
    RepeatedOutputBase,
    #[error("could not resolve a Bazel executable for {0}")]
    ExecutableNotFound(PathBuf),
    #[error("could not resolve the Aspect CLI executable: {0}")]
    AspectExecutableNotFound(PathBuf),
    #[error("Aspect command argument overrides a server-owned setting: {0}")]
    AspectReservedArgument(String),
    #[error("Aspect lint --fix is disabled; set aspect.allow_workspace_mutation = true to opt in")]
    AspectWorkspaceMutationDenied,
    #[error("resolved Bazel executable would recursively launch the agent-mode shim: {0}")]
    ExecutableRecursion(PathBuf),
    #[error("invalid redaction expression {pattern:?}: {source}")]
    InvalidRedaction {
        pattern: String,
        source: regex::Error,
    },
    #[error(transparent)]
    Io(#[from] io::Error),
}
