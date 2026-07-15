use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

use bazel_mcp_types::{BazelCommand, CommandClass};

use crate::PolicyError;

pub const INTERNAL_BEP_FLAG: &str = "--build_event_binary_file";

const MAX_ARGUMENT_BYTES: usize = 64 * 1024;
const MAX_ARGUMENT_COUNT: usize = 10_000;
const MAX_ARGUMENTS_BYTES: usize = 2 * 1024 * 1024;

const RESERVED_FLAGS: &[&str] = &[
    "--build_event_binary_file",
    "--build_event_binary_file_path_conversion",
    "--build_event_publish_all_actions",
    "--build_event_json_file",
    "--build_event_text_file",
    "--color",
    "--curses",
    "--invocation_id",
    "--show_progress",
    "--show_result",
    "--test_output",
    "--test_summary",
    "--tool_tag",
];

pub fn validate_arguments(arguments: &[String]) -> Result<(), PolicyError> {
    if arguments.len() > MAX_ARGUMENT_COUNT {
        return Err(PolicyError::TooManyArguments {
            maximum_count: MAX_ARGUMENT_COUNT,
        });
    }
    let mut total_bytes = 0_usize;
    for argument in arguments {
        if argument.as_bytes().contains(&0) {
            return Err(PolicyError::InvalidArgument);
        }
        if argument.len() > MAX_ARGUMENT_BYTES {
            return Err(PolicyError::ArgumentTooLong {
                maximum_bytes: MAX_ARGUMENT_BYTES,
            });
        }
        total_bytes = total_bytes.saturating_add(argument.len());
        if total_bytes > MAX_ARGUMENTS_BYTES {
            return Err(PolicyError::ArgumentsTooLarge {
                maximum_bytes: MAX_ARGUMENTS_BYTES,
            });
        }
        if let Some(flag) = RESERVED_FLAGS
            .iter()
            .find(|flag| argument == **flag || argument.starts_with(&format!("{flag}=")))
        {
            return Err(PolicyError::ReservedFlag((*flag).to_owned()));
        }
    }
    Ok(())
}

pub fn validate_query_arguments(
    command: &BazelCommand,
    arguments: &[String],
) -> Result<(), PolicyError> {
    if command.class() != CommandClass::Query {
        return Ok(());
    }
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];
        let output = if argument == "--output" {
            index = index.saturating_add(1);
            arguments.get(index).map(String::as_str)
        } else {
            argument.strip_prefix("--output=")
        };
        if output.is_some_and(|output| {
            matches!(
                output,
                "proto" | "streamed_proto" | "jsonproto" | "streamed_jsonproto"
            )
        }) {
            return Err(PolicyError::IncompatibleQueryOutput(
                output.unwrap_or_default().to_owned(),
            ));
        }
        index = index.saturating_add(1);
    }
    Ok(())
}

/// Returns the canonical lock key selected by an explicit Bazel output base.
///
/// Bazel accepts the startup option in both `--output_base=/path` and
/// `--output_base /path` forms. The caller is allowed to choose it, but two
/// invocations that choose the same path must share a scheduler lock.
pub fn effective_output_base(
    workspace: &Path,
    startup_arguments: &[String],
) -> Result<Option<PathBuf>, PolicyError> {
    let mut selected = None;
    let mut index = 0;
    while index < startup_arguments.len() {
        let argument = &startup_arguments[index];
        let value = if argument == "--output_base" {
            index += 1;
            Some(
                startup_arguments
                    .get(index)
                    .ok_or(PolicyError::MissingOutputBaseValue)?
                    .as_str(),
            )
        } else {
            argument.strip_prefix("--output_base=")
        };
        if let Some(value) = value {
            if selected.is_some() {
                return Err(PolicyError::RepeatedOutputBase);
            }
            let path = PathBuf::from(value);
            let absolute = if path.is_absolute() {
                path
            } else {
                workspace.join(path)
            };
            selected = Some(canonicalize_with_missing_tail(&absolute)?);
        }
        index += 1;
    }
    Ok(selected)
}

fn canonicalize_with_missing_tail(path: &Path) -> Result<PathBuf, PolicyError> {
    let normalized = normalize_absolute(path);
    let mut existing = normalized.as_path();
    let mut missing = Vec::<OsString>::new();
    loop {
        match std::fs::symlink_metadata(existing) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = existing
                    .file_name()
                    .ok_or_else(|| std::io::Error::other("output base has no existing ancestor"))?;
                missing.push(name.to_owned());
                existing = existing
                    .parent()
                    .ok_or_else(|| std::io::Error::other("output base has no existing ancestor"))?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    let mut canonical = existing.canonicalize()?;
    for component in missing.into_iter().rev() {
        canonical.push(component);
    }
    Ok(canonical)
}

fn normalize_absolute(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_both_reserved_flag_forms() {
        assert!(validate_arguments(&["--output_base=/tmp/x".into()]).is_ok());
        assert!(validate_arguments(&["--build_event_binary_file".into()]).is_err());
        assert!(validate_arguments(&["--test_output=streamed".into()]).is_err());
        assert!(validate_arguments(&["--tool_tag".into(), "other".into()]).is_err());
        assert!(validate_arguments(&["//safe:target".into()]).is_ok());
        assert!(validate_arguments(&["x".repeat(MAX_ARGUMENT_BYTES + 1)]).is_err());
        assert!(validate_arguments(&vec![String::new(); MAX_ARGUMENT_COUNT + 1]).is_err());
    }

    #[test]
    fn extracts_both_output_base_forms_and_rejects_ambiguity() {
        let workspace = std::env::current_dir().unwrap();
        let equals = effective_output_base(&workspace, &["--output_base=target/one".to_owned()])
            .unwrap()
            .unwrap();
        let split = effective_output_base(
            &workspace,
            &["--output_base".to_owned(), "target/one".to_owned()],
        )
        .unwrap()
        .unwrap();
        assert_eq!(equals, split);
        assert!(
            effective_output_base(
                &workspace,
                &[
                    "--output_base=one".to_owned(),
                    "--output_base=two".to_owned(),
                ],
            )
            .is_err()
        );

        let normalized =
            effective_output_base(&workspace, &["--output_base=missing/../shared".to_owned()])
                .unwrap()
                .unwrap();
        let direct = effective_output_base(&workspace, &["--output_base=shared".to_owned()])
            .unwrap()
            .unwrap();
        assert_eq!(normalized, direct);
    }

    #[test]
    fn rejects_query_formats_that_break_line_streaming() {
        for arguments in [
            vec!["--output=proto".to_owned()],
            vec!["--output".to_owned(), "streamed_jsonproto".to_owned()],
        ] {
            assert!(validate_query_arguments(&BazelCommand::Query, &arguments).is_err());
        }
        assert!(
            validate_query_arguments(&BazelCommand::Cquery, &["--output=label".to_owned()]).is_ok()
        );
        assert!(
            validate_query_arguments(&BazelCommand::Build, &["--output=proto".to_owned()]).is_ok()
        );
    }
}
