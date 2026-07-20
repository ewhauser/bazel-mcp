use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use regex::Regex;
use schemars::{JsonSchema, Schema, schema_for};
use serde::{Deserialize, Serialize};

use crate::CASE_SCHEMA_VERSION;

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CaseManifest {
    pub schema_version: u32,
    pub id: String,
    #[serde(default)]
    pub issue: Option<u64>,
    pub workspace: PathBuf,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub startup_args: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default = "default_platforms")]
    pub platforms: Vec<String>,
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
    #[serde(default)]
    pub bazel_version: Option<String>,
    #[serde(default)]
    pub test_target: Option<String>,
    #[serde(default)]
    pub evidence: Option<EvidenceSpec>,
    #[serde(default)]
    pub replay: ReplaySpec,
    pub expect: CaseExpectation,
    pub provenance: ProvenanceSpec,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceSpec {
    #[serde(default = "default_bep")]
    pub bep: PathBuf,
    #[serde(default = "default_stdout")]
    pub stdout: PathBuf,
    #[serde(default = "default_stderr")]
    pub stderr: PathBuf,
    #[serde(default = "default_exit")]
    pub exit: PathBuf,
    #[serde(default)]
    pub test_logs: Vec<PathBuf>,
    #[serde(default = "default_expected")]
    pub expected: PathBuf,
    #[serde(default = "default_provenance")]
    pub provenance: PathBuf,
}

impl Default for EvidenceSpec {
    fn default() -> Self {
        Self {
            bep: default_bep(),
            stdout: default_stdout(),
            stderr: default_stderr(),
            exit: default_exit(),
            test_logs: Vec::new(),
            expected: default_expected(),
            provenance: default_provenance(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReplaySpec {
    pub(crate) max_items: usize,
    pub(crate) max_bytes: usize,
}

impl Default for ReplaySpec {
    fn default() -> Self {
        Self {
            max_items: 100,
            max_bytes: 64 * 1024,
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CaseExpectation {
    pub state: String,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub headline_equals: Option<String>,
    #[serde(default)]
    pub headline_contains: Option<String>,
    #[serde(default)]
    pub inspect_hint: Option<String>,
    #[serde(default = "default_max_visible_bytes")]
    pub max_visible_bytes: usize,
    #[serde(default = "default_max_diagnostics")]
    pub max_diagnostics: usize,
    #[serde(default)]
    pub diagnostics: Vec<DiagnosticExpectation>,
    #[serde(default)]
    pub artifacts: Vec<ArtifactExpectation>,
    #[serde(default)]
    pub absent: AbsentExpectation,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticExpectation {
    #[serde(default)]
    pub(crate) rank: Option<usize>,
    #[serde(default)]
    pub(crate) severity: Option<String>,
    #[serde(default)]
    pub(crate) category: Option<String>,
    #[serde(default)]
    pub(crate) message_equals: Option<String>,
    #[serde(default)]
    pub(crate) message_prefix: Option<String>,
    #[serde(default)]
    pub(crate) message_contains: Option<String>,
    #[serde(default)]
    pub(crate) path: Option<String>,
    #[serde(default)]
    pub(crate) line: Option<u32>,
    #[serde(default)]
    pub(crate) column: Option<u32>,
    #[serde(default)]
    pub(crate) target: Option<String>,
    #[serde(default)]
    pub(crate) action: Option<String>,
    #[serde(default)]
    pub(crate) repetition_count: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactExpectation {
    #[serde(default)]
    pub(crate) name_equals: Option<String>,
    #[serde(default)]
    pub(crate) name_contains: Option<String>,
    #[serde(default)]
    pub(crate) kind: Option<String>,
    #[serde(default)]
    pub(crate) uri_contains: Option<String>,
    #[serde(default)]
    pub(crate) locally_available: Option<bool>,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AbsentExpectation {
    pub(crate) message_contains: Vec<String>,
    pub(crate) path_contains: Vec<String>,
    pub(crate) target_contains: Vec<String>,
    pub(crate) artifact_uri_contains: Vec<String>,
    pub(crate) raw_contains: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProvenanceSpec {
    pub tool: String,
    pub tool_version: String,
    #[serde(default)]
    pub rules: BTreeMap<String, String>,
    #[serde(default)]
    pub origin_repository: Option<String>,
    #[serde(default)]
    pub origin_commit: Option<String>,
}

#[derive(Clone, Debug)]
pub struct LoadedCase {
    pub manifest_path: PathBuf,
    pub directory: PathBuf,
    pub manifest: CaseManifest,
}

impl LoadedCase {
    pub fn evidence_path(&self, relative: &Path) -> PathBuf {
        self.directory.join(relative)
    }
}

impl CaseManifest {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == CASE_SCHEMA_VERSION,
            "case {} uses schema {}, expected {}",
            self.id,
            self.schema_version,
            CASE_SCHEMA_VERSION
        );
        let id_pattern = Regex::new(r"^[a-z0-9][a-z0-9._/-]*$").expect("valid case ID regex");
        ensure!(
            id_pattern.is_match(&self.id),
            "invalid case ID {:?}",
            self.id
        );
        validate_relative_path(&self.workspace, "workspace")?;
        ensure!(
            matches!(
                self.command.as_str(),
                "build" | "test" | "coverage" | "query" | "cquery" | "aquery"
            ),
            "case {} has unsupported command {:?}",
            self.id,
            self.command
        );
        ensure!(
            self.timeout_seconds > 0,
            "case {} timeout must be positive",
            self.id
        );
        ensure!(
            (512..=65_536).contains(&self.expect.max_visible_bytes),
            "case {} max_visible_bytes must be between 512 and 65536",
            self.id
        );
        ensure!(
            (1..=1_000).contains(&self.expect.max_diagnostics),
            "case {} max_diagnostics must be between 1 and 1000",
            self.id
        );
        ensure!(
            self.replay.max_items > 0 && self.replay.max_bytes > 0,
            "case {} replay budgets must be positive",
            self.id
        );
        let states = [
            "succeeded",
            "failed",
            "timed_out",
            "cancelled",
            "interrupted",
        ];
        ensure!(
            states.contains(&self.expect.state.as_str()),
            "case {} has unsupported expected state {:?}",
            self.id,
            self.expect.state
        );
        ensure!(
            !self.platforms.is_empty(),
            "case {} has no platforms",
            self.id
        );
        let mut platforms = BTreeSet::new();
        for platform in &self.platforms {
            ensure!(
                matches!(platform.as_str(), "any" | "linux" | "macos" | "windows"),
                "case {} has invalid platform {platform:?}",
                self.id
            );
            ensure!(
                platforms.insert(platform),
                "case {} repeats platform {platform:?}",
                self.id
            );
        }
        let mut tags = BTreeSet::new();
        for tag in &self.tags {
            ensure!(!tag.is_empty(), "case {} has an empty tag", self.id);
            ensure!(tags.insert(tag), "case {} repeats tag {tag:?}", self.id);
        }
        let mut ranks = BTreeSet::new();
        for diagnostic in &self.expect.diagnostics {
            ensure!(
                diagnostic.message_equals.is_some()
                    || diagnostic.message_prefix.is_some()
                    || diagnostic.message_contains.is_some()
                    || diagnostic.path.is_some()
                    || diagnostic.category.is_some(),
                "case {} has a diagnostic expectation with no selector",
                self.id
            );
            if let Some(rank) = diagnostic.rank {
                ensure!(
                    ranks.insert(rank),
                    "case {} repeats diagnostic rank {rank}",
                    self.id
                );
            }
            if let Some(severity) = &diagnostic.severity {
                ensure!(
                    matches!(severity.as_str(), "error" | "warning" | "note"),
                    "case {} has invalid severity {severity:?}",
                    self.id
                );
            }
            if let Some(category) = &diagnostic.category {
                ensure!(
                    matches!(
                        category.as_str(),
                        "workspace"
                            | "loading"
                            | "analysis"
                            | "visibility"
                            | "action"
                            | "compilation"
                            | "test"
                            | "bazel"
                            | "unknown"
                    ),
                    "case {} has invalid diagnostic category {category:?}",
                    self.id
                );
            }
        }
        if let Some(evidence) = &self.evidence {
            let mut paths = BTreeSet::new();
            for (name, path) in evidence.paths() {
                validate_relative_path(path, name)?;
                ensure!(
                    paths.insert(path),
                    "case {} reuses evidence path {}",
                    self.id,
                    path.display()
                );
            }
            ensure!(
                evidence.test_logs.is_empty() || self.test_target.is_some(),
                "case {} records test logs without test_target",
                self.id
            );
        }
        ensure!(
            self.test_target.is_none() || self.command == "test" || self.command == "coverage",
            "case {} sets test_target for non-test command {:?}",
            self.id,
            self.command
        );
        ensure!(
            !self.provenance.tool.trim().is_empty(),
            "case {} has no tool",
            self.id
        );
        ensure!(
            !self.provenance.tool_version.trim().is_empty(),
            "case {} has no tool version",
            self.id
        );
        match (
            &self.provenance.origin_repository,
            &self.provenance.origin_commit,
        ) {
            (Some(repository), Some(commit)) => {
                ensure!(
                    !repository.trim().is_empty(),
                    "case {} has an empty origin",
                    self.id
                );
                ensure!(
                    !commit.trim().is_empty(),
                    "case {} has an empty origin commit",
                    self.id
                );
            }
            (None, None) => {}
            _ => bail!(
                "case {} must set both origin_repository and origin_commit",
                self.id
            ),
        }
        Ok(())
    }

    pub fn supports_current_platform(&self) -> bool {
        self.platforms.iter().any(|platform| {
            platform == "any"
                || (platform == "linux" && cfg!(target_os = "linux"))
                || (platform == "macos" && cfg!(target_os = "macos"))
                || (platform == "windows" && cfg!(target_os = "windows"))
        })
    }
}

impl EvidenceSpec {
    pub fn paths(&self) -> Vec<(&'static str, &Path)> {
        let mut paths = vec![
            ("evidence.bep", self.bep.as_path()),
            ("evidence.stdout", self.stdout.as_path()),
            ("evidence.stderr", self.stderr.as_path()),
            ("evidence.exit", self.exit.as_path()),
            ("evidence.expected", self.expected.as_path()),
            ("evidence.provenance", self.provenance.as_path()),
        ];
        paths.extend(
            self.test_logs
                .iter()
                .map(|path| ("evidence.test_logs", path.as_path())),
        );
        paths
    }
}

pub fn schema() -> Schema {
    schema_for!(CaseManifest)
}

pub fn discover_cases(corpus_root: &Path) -> Result<Vec<LoadedCase>> {
    let mut manifests = Vec::new();
    collect_manifests(corpus_root, &mut manifests)?;
    manifests.sort();
    let mut ids = BTreeMap::<String, PathBuf>::new();
    let mut cases = Vec::new();
    for manifest_path in manifests {
        let contents = fs::read_to_string(&manifest_path)
            .with_context(|| format!("read case manifest {}", manifest_path.display()))?;
        let manifest: CaseManifest = toml::from_str(&contents)
            .with_context(|| format!("parse case manifest {}", manifest_path.display()))?;
        manifest
            .validate()
            .with_context(|| format!("validate case manifest {}", manifest_path.display()))?;
        if let Some(previous) = ids.insert(manifest.id.clone(), manifest_path.clone()) {
            bail!(
                "duplicate reducer case ID {:?} in {} and {}",
                manifest.id,
                previous.display(),
                manifest_path.display()
            );
        }
        let directory = manifest_path
            .parent()
            .context("case manifest has no parent directory")?
            .to_owned();
        let expected_id = directory
            .strip_prefix(corpus_root)
            .with_context(|| format!("case directory {} is outside corpus", directory.display()))?
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        ensure!(
            manifest.id == expected_id,
            "case ID {:?} must match its corpus directory {:?}",
            manifest.id,
            expected_id
        );
        cases.push(LoadedCase {
            manifest_path,
            directory,
            manifest,
        });
    }
    Ok(cases)
}

pub fn find_repository_root(start: &Path) -> Result<PathBuf> {
    let start = start
        .canonicalize()
        .with_context(|| format!("canonicalize {}", start.display()))?;
    for directory in start.ancestors() {
        if directory.join("Cargo.toml").is_file() && directory.join("MODULE.bazel").is_file() {
            return Ok(directory.to_owned());
        }
    }
    bail!(
        "could not find bazel-mcp repository root from {}",
        start.display()
    )
}

fn collect_manifests(directory: &Path, manifests: &mut Vec<PathBuf>) -> Result<()> {
    if !directory.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(directory)
        .with_context(|| format!("read corpus directory {}", directory.display()))?
    {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            if entry.file_name() == "case.toml" && fs::metadata(entry.path())?.is_file() {
                manifests.push(entry.path());
            }
            continue;
        }
        let path = entry.path();
        if file_type.is_dir() {
            collect_manifests(&path, manifests)?;
        } else if file_type.is_file() && entry.file_name() == "case.toml" {
            manifests.push(path);
        }
    }
    Ok(())
}

fn validate_relative_path(path: &Path, field: &str) -> Result<()> {
    ensure!(!path.as_os_str().is_empty(), "{field} cannot be empty");
    ensure!(
        !path.is_absolute(),
        "{field} must be relative: {}",
        path.display()
    );
    ensure!(
        path.components()
            .all(|component| matches!(component, Component::Normal(_))),
        "{field} cannot contain dot or parent components: {}",
        path.display()
    );
    Ok(())
}

const fn default_timeout_seconds() -> u64 {
    300
}

fn default_platforms() -> Vec<String> {
    vec!["any".to_owned()]
}

const fn default_max_visible_bytes() -> usize {
    8192
}

const fn default_max_diagnostics() -> usize {
    20
}

fn default_bep() -> PathBuf {
    "events.bep".into()
}

fn default_stdout() -> PathBuf {
    "stdout.txt".into()
}

fn default_stderr() -> PathBuf {
    "stderr.txt".into()
}

fn default_exit() -> PathBuf {
    "exit.code".into()
}

fn default_expected() -> PathBuf {
    "expected.json".into()
}

fn default_provenance() -> PathBuf {
    "provenance.json".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_manifest() -> CaseManifest {
        CaseManifest {
            schema_version: CASE_SCHEMA_VERSION,
            id: "cpp/compiler/type-mismatch".to_owned(),
            issue: None,
            workspace: "examples/cpp".into(),
            command: "build".to_owned(),
            args: vec!["//cases:type_mismatch".to_owned()],
            startup_args: Vec::new(),
            tags: vec!["cpp".to_owned()],
            platforms: vec!["linux".to_owned(), "macos".to_owned()],
            timeout_seconds: 60,
            bazel_version: Some("9.2.0".to_owned()),
            test_target: None,
            evidence: Some(EvidenceSpec::default()),
            replay: ReplaySpec::default(),
            expect: CaseExpectation {
                state: "failed".to_owned(),
                exit_code: Some(1),
                headline_equals: None,
                headline_contains: Some("conversion".to_owned()),
                inspect_hint: None,
                max_visible_bytes: 8192,
                max_diagnostics: 20,
                diagnostics: vec![DiagnosticExpectation {
                    rank: Some(0),
                    severity: Some("error".to_owned()),
                    category: Some("compilation".to_owned()),
                    message_equals: None,
                    message_prefix: None,
                    message_contains: Some("conversion".to_owned()),
                    path: Some("cases/type_mismatch.cc".to_owned()),
                    line: Some(5),
                    column: None,
                    target: None,
                    action: None,
                    repetition_count: None,
                }],
                artifacts: Vec::new(),
                absent: AbsentExpectation::default(),
            },
            provenance: ProvenanceSpec {
                tool: "clang".to_owned(),
                tool_version: "17".to_owned(),
                rules: BTreeMap::new(),
                origin_repository: None,
                origin_commit: None,
            },
        }
    }

    #[test]
    fn validates_a_complete_manifest() {
        valid_manifest().validate().unwrap();
    }

    #[test]
    fn rejects_paths_that_escape_the_case_or_repository() {
        let mut manifest = valid_manifest();
        manifest.evidence.as_mut().unwrap().stderr = "../secret".into();
        assert!(
            manifest
                .validate()
                .unwrap_err()
                .to_string()
                .contains("parent")
        );
    }

    #[test]
    fn rejects_unknown_manifest_fields() {
        let text = r#"
schema_version = 1
id = "example/case"
workspace = "examples/example"
command = "build"
unknown = true

[expect]
state = "failed"

[provenance]
tool = "example"
tool_version = "1"
"#;
        assert!(toml::from_str::<CaseManifest>(text).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn discovers_case_manifests_in_bazel_symlink_runfiles() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("manifest-source.toml");
        fs::write(&source, toml::to_string(&valid_manifest()).unwrap()).unwrap();
        let case_directory = root.path().join("cpp/compiler/type-mismatch");
        fs::create_dir_all(&case_directory).unwrap();
        symlink(source, case_directory.join("case.toml")).unwrap();

        let cases = discover_cases(root.path()).unwrap();
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].manifest.id, "cpp/compiler/type-mismatch");
    }
}
