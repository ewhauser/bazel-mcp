//! Contained local-artifact resolution and coverage loading.

use std::{
    io,
    path::{Path, PathBuf},
};

use bazel_mcp_reducer::parse_lcov_reader;
use bazel_mcp_types::ArtifactKind;

use crate::service::InvocationService;

impl InvocationService {
    pub(crate) async fn load_coverage(
        &self,
        workspace: &Path,
        artifacts: &[bazel_mcp_types::Artifact],
    ) -> Option<bazel_mcp_types::CoverageSummary> {
        for artifact in artifacts
            .iter()
            .filter(|artifact| artifact.kind == ArtifactKind::Coverage)
        {
            let Some(canonical) = self.validated_artifact_path(workspace, artifact).await else {
                continue;
            };
            let parsed = tokio::task::spawn_blocking(move || {
                let file = std::fs::File::open(canonical)?;
                parse_lcov_reader(std::io::BufReader::new(file))
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
            })
            .await;
            if let Ok(Ok(coverage)) = parsed {
                return Some(coverage);
            }
        }
        None
    }

    pub(crate) async fn validated_artifact_path(
        &self,
        workspace: &Path,
        artifact: &bazel_mcp_types::Artifact,
    ) -> Option<PathBuf> {
        let path = local_artifact_path(artifact)?;
        let canonical = tokio::fs::canonicalize(&path).await.ok()?;
        let in_workspace = canonical.starts_with(workspace);
        let in_output_root = if let Some(root) = &self.config.output_user_root {
            tokio::fs::canonicalize(root)
                .await
                .is_ok_and(|root| canonical.starts_with(root))
        } else {
            false
        };
        let in_bazel_testlogs = if artifact.kind == ArtifactKind::TestLog {
            match bazel_test_log_output_base(&path).zip(bazel_test_log_output_base(&canonical)) {
                Some((lexical_root, canonical_root)) => tokio::fs::canonicalize(lexical_root)
                    .await
                    .is_ok_and(|lexical_root| {
                        lexical_root == canonical_root && canonical.starts_with(&lexical_root)
                    }),
                None => false,
            }
        } else {
            false
        };
        (in_workspace || in_output_root || in_bazel_testlogs).then_some(canonical)
    }
}

pub(crate) fn local_artifact_path(artifact: &bazel_mcp_types::Artifact) -> Option<PathBuf> {
    if !artifact.locally_available {
        return None;
    }
    if let Some(path) = artifact.uri.strip_prefix("file://") {
        return Some(PathBuf::from(path));
    }
    let path = PathBuf::from(&artifact.uri);
    path.is_absolute().then_some(path)
}

fn bazel_test_log_output_base(path: &Path) -> Option<PathBuf> {
    let components = path.components().collect::<Vec<_>>();
    let execroot = components
        .iter()
        .position(|component| component.as_os_str() == "execroot")?;
    let bazel_out = components
        .iter()
        .enumerate()
        .skip(execroot + 1)
        .find(|(_, component)| component.as_os_str() == "bazel-out")?
        .0;
    components
        .iter()
        .enumerate()
        .skip(bazel_out + 1)
        .find(|(_, component)| component.as_os_str() == "testlogs")?;
    if !matches!(
        path.extension().and_then(|value| value.to_str()),
        Some("log" | "xml")
    ) {
        return None;
    }
    Some(components[..execroot].iter().collect())
}
