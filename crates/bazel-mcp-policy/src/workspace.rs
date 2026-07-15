use std::path::{Path, PathBuf};

use crate::PolicyError;

const WORKSPACE_MARKERS: &[&str] = &["MODULE.bazel", "WORKSPACE.bazel", "WORKSPACE"];

pub fn validate_workspace(
    workspace: &Path,
    allowed_roots: &[PathBuf],
) -> Result<PathBuf, PolicyError> {
    if !workspace.is_absolute() {
        return Err(PolicyError::WorkspaceNotAbsolute(workspace.to_owned()));
    }
    let canonical = workspace.canonicalize()?;
    if !allowed_roots.is_empty() {
        let allowed = allowed_roots.iter().any(|root| {
            root.canonicalize()
                .is_ok_and(|canonical_root| canonical.starts_with(canonical_root))
        });
        if !allowed {
            return Err(PolicyError::WorkspaceNotAllowed(canonical));
        }
    }
    if !WORKSPACE_MARKERS
        .iter()
        .any(|marker| canonical.join(marker).is_file())
    {
        return Err(PolicyError::NotBazelWorkspace(canonical));
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn canonicalizes_an_allowed_bazel_workspace() {
        let root = tempdir().unwrap();
        let workspace = root.path().join("repo");
        fs::create_dir(&workspace).unwrap();
        fs::write(workspace.join("MODULE.bazel"), "").unwrap();
        assert_eq!(
            validate_workspace(&workspace, &[root.path().to_owned()]).unwrap(),
            workspace.canonicalize().unwrap()
        );
    }

    #[test]
    fn accepts_a_bazel_workspace_when_roots_are_unrestricted() {
        let root = tempdir().unwrap();
        let workspace = root.path().join("repo");
        fs::create_dir(&workspace).unwrap();
        fs::write(workspace.join("MODULE.bazel"), "").unwrap();

        assert_eq!(
            validate_workspace(&workspace, &[]).unwrap(),
            workspace.canonicalize().unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_workspace_symlink_that_escapes_an_allowed_root() {
        use std::os::unix::fs::symlink;

        let allowed = tempdir().unwrap();
        let outside = tempdir().unwrap();
        fs::write(outside.path().join("MODULE.bazel"), "").unwrap();
        let link = allowed.path().join("escaped-workspace");
        symlink(outside.path(), &link).unwrap();
        assert!(validate_workspace(&link, &[allowed.path().to_owned()]).is_err());
    }
}
