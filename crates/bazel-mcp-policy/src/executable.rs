use std::path::{Path, PathBuf};

use crate::{PolicyConfig, PolicyError};

pub fn resolve_bazel_executable(
    workspace: &Path,
    config: &PolicyConfig,
) -> Result<PathBuf, PolicyError> {
    if let Some(path) = &config.bazel_executable {
        return executable(path).ok_or_else(|| PolicyError::ExecutableNotFound(path.clone()));
    }
    let wrapper = workspace.join("tools/bazel");
    if let Some(path) = executable(&wrapper) {
        return Ok(path);
    }
    for name in ["bazelisk", "bazel"] {
        if let Ok(path) = which::which(name) {
            return Ok(path);
        }
    }
    Err(PolicyError::ExecutableNotFound(workspace.to_owned()))
}

/// Resolve the Aspect CLI launcher used for configured Aspect commands.
///
/// Unlike Bazel discovery, Aspect does not use a workspace-local wrapper. An
/// explicit path wins; otherwise the `aspect` executable is resolved from the
/// server process's `PATH`.
pub fn resolve_aspect_executable(configured: Option<&Path>) -> Result<PathBuf, PolicyError> {
    if let Some(path) = configured {
        return executable(path)
            .ok_or_else(|| PolicyError::AspectExecutableNotFound(path.to_owned()));
    }
    let path = std::env::var_os("PATH");
    let current_dir = std::env::current_dir()
        .map_err(|_| PolicyError::AspectExecutableNotFound(PathBuf::from("aspect")))?;
    resolve_aspect_on_path(path.as_deref(), &current_dir)
}

fn resolve_aspect_on_path(
    path: Option<&std::ffi::OsStr>,
    current_dir: &Path,
) -> Result<PathBuf, PolicyError> {
    which::which_in("aspect", path, current_dir)
        .map_err(|_| PolicyError::AspectExecutableNotFound(PathBuf::from("aspect")))
}

fn executable(path: &Path) -> Option<PathBuf> {
    let metadata = path.metadata().ok()?;
    if !metadata.is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return None;
        }
    }
    Some(path.to_owned())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[cfg(unix)]
    #[test]
    fn ignores_non_executable_wrapper_files() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempdir().unwrap();
        let wrapper = root.path().join("bazel");
        fs::write(&wrapper, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(executable(&wrapper), None);
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(executable(&wrapper), Some(wrapper));
    }

    #[cfg(unix)]
    #[test]
    fn resolves_an_explicit_aspect_executable() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempdir().unwrap();
        let aspect = root.path().join("aspect");
        fs::write(&aspect, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&aspect, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(resolve_aspect_executable(Some(&aspect)).unwrap(), aspect);
    }

    #[cfg(unix)]
    #[test]
    fn resolves_aspect_from_the_search_path_when_not_configured() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempdir().unwrap();
        let aspect = root.path().join("aspect");
        fs::write(&aspect, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&aspect, fs::Permissions::from_mode(0o700)).unwrap();

        assert_eq!(
            resolve_aspect_on_path(Some(root.path().as_os_str()), root.path()).unwrap(),
            aspect
        );
    }
}
