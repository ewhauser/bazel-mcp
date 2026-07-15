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
}
