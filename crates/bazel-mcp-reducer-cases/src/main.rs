use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, ensure};
use bazel_mcp_reducer_cases::{
    EvidenceSpec, LiveOptions, LoadedCase, discover_cases, find_repository_root, observe_replay,
    replay_with_evidence, run_live_case, sanitize_binary, sanitize_text, schema,
    verify_case_contract, verify_case_evidence, verify_expectations, verify_live_replay_parity,
    verify_recorded_case, verify_sanitized_evidence,
};
use clap::{Args, Parser, Subcommand};
use serde::Serialize;

const ACCEPT_ENVIRONMENT: &str = "BAZEL_MCP_ACCEPT_REDUCER_CASES";

#[derive(Debug, Parser)]
#[command(
    name = "reducer-cases",
    version,
    about = "Record and verify manifest-driven Bazel reducer integration cases"
)]
struct Cli {
    /// bazel-mcp repository root. Defaults to discovery from the current directory.
    #[arg(long, global = true)]
    repository: Option<PathBuf>,
    /// Corpus directory, relative to the repository unless absolute.
    #[arg(long, global = true, default_value = "testdata/reducer-corpus")]
    corpus: PathBuf,
    #[command(subcommand)]
    command: CaseCommand,
}

#[derive(Debug, Subcommand)]
enum CaseCommand {
    /// List discovered, validated cases.
    List {
        #[command(flatten)]
        selection: Selection,
        #[arg(long)]
        json: bool,
    },
    /// Replay checked evidence and optionally run live MCP verification.
    Verify {
        #[command(flatten)]
        selection: Selection,
        #[arg(long)]
        live: bool,
        #[command(flatten)]
        live_options: LiveArguments,
    },
    /// Run one live case and write sanitized actual evidence without blessing it.
    Record {
        #[arg(long = "case")]
        case_id: String,
        /// Recompute only the exact golden from existing sanitized evidence.
        #[arg(long)]
        replay_only: bool,
        #[command(flatten)]
        live_options: LiveArguments,
    },
    /// Accept a previously recorded actual result after explicit review.
    Accept {
        #[arg(long = "case")]
        case_id: String,
        /// Required together with BAZEL_MCP_ACCEPT_REDUCER_CASES=1.
        #[arg(long)]
        yes: bool,
    },
    /// Print the versioned JSON schema for case.toml.
    Schema,
}

#[derive(Clone, Debug, Default, Args)]
struct Selection {
    /// Select exact case IDs. May be repeated.
    #[arg(long = "case")]
    case_ids: Vec<String>,
    /// Select cases carrying any of these tags. May be repeated.
    #[arg(long)]
    tag: Vec<String>,
    /// Select cases whose example workspace exactly matches this path. May be repeated.
    #[arg(long)]
    workspace: Vec<PathBuf>,
    /// Select cases affected by changes since this git ref.
    #[arg(long)]
    changed_from: Option<String>,
    /// Succeed when discovery filters select no cases (useful for CI shards).
    #[arg(long)]
    allow_empty: bool,
}

#[derive(Clone, Debug, Args)]
struct LiveArguments {
    /// bazel-mcp server executable.
    #[arg(long, env = "BAZEL_MCP_SERVER_BIN")]
    server: Option<PathBuf>,
    /// Explicit Bazel or Bazelisk executable used by the server.
    #[arg(long, env = "BAZEL_MCP_BAZEL")]
    bazel: Option<PathBuf>,
    /// Parent for isolated runtime directories.
    #[arg(long)]
    runtime_parent: Option<PathBuf>,
    /// Override the manifest's Bazel version for compatibility-matrix runs.
    #[arg(long)]
    bazel_version: Option<String>,
}

#[derive(Serialize)]
struct ListedCase<'a> {
    id: &'a str,
    issue: Option<u64>,
    workspace: &'a Path,
    command: &'a str,
    tags: &'a [String],
    platforms: &'a [String],
    recorded: bool,
}

#[derive(Serialize)]
struct RecordedProvenance<'a> {
    schema_version: u32,
    case: &'a str,
    workspace: &'a Path,
    command: &'a str,
    args: &'a [String],
    bazel_version: &'a Option<String>,
    platform: &'static str,
    architecture: &'static str,
    tool: &'a str,
    tool_version: &'a str,
    rules: &'a BTreeMap<String, String>,
    origin_repository: &'a Option<String>,
    origin_commit: &'a Option<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let current = env::current_dir().context("read current directory")?;
    let repository = cli
        .repository
        .map_or_else(|| find_repository_root(&current), canonicalize)?;
    let corpus = if cli.corpus.is_absolute() {
        cli.corpus
    } else {
        repository.join(cli.corpus)
    };
    if matches!(cli.command, CaseCommand::Schema) {
        println!("{}", serde_json::to_string_pretty(&schema())?);
        return Ok(());
    }
    let cases = discover_cases(&corpus)?;
    ensure!(
        !cases.is_empty(),
        "no reducer cases found under {}",
        corpus.display()
    );

    match cli.command {
        CaseCommand::List { selection, json } => {
            let selected = select_cases(&repository, &cases, &selection)?;
            if json {
                let listed = selected
                    .iter()
                    .map(|case| ListedCase {
                        id: &case.manifest.id,
                        issue: case.manifest.issue,
                        workspace: &case.manifest.workspace,
                        command: &case.manifest.command,
                        tags: &case.manifest.tags,
                        platforms: &case.manifest.platforms,
                        recorded: case.manifest.evidence.is_some(),
                    })
                    .collect::<Vec<_>>();
                println!("{}", serde_json::to_string_pretty(&listed)?);
            } else {
                for case in selected {
                    println!(
                        "{}\t{}\t{}\t{}",
                        case.manifest.id,
                        case.manifest.workspace.display(),
                        case.manifest.command,
                        case.manifest.tags.join(",")
                    );
                }
            }
        }
        CaseCommand::Verify {
            selection,
            live,
            live_options,
        } => {
            let selected = select_cases(&repository, &cases, &selection)?;
            let options = live
                .then(|| resolve_live_options(&repository, &live_options))
                .transpose()?;
            for case in selected {
                verify_recorded(case)?;
                println!("replay\t{}\tok", case.manifest.id);
                if let Some(options) = &options {
                    if case.manifest.supports_current_platform() {
                        let run = run_live_case(case, options)?;
                        verify_expectations(
                            &case.manifest.id,
                            &case.manifest.expect,
                            &run.observation,
                        )?;
                        println!("live\t{}\tok\t{}", case.manifest.id, run.invocation_id);
                    } else {
                        println!("live\t{}\tskipped-platform", case.manifest.id);
                    }
                }
            }
        }
        CaseCommand::Record {
            case_id,
            replay_only,
            live_options,
        } => {
            let case = exact_case(&cases, &case_id)?;
            if replay_only {
                write_actual_replay(case)?;
                println!(
                    "recorded-replay\t{}\nreview actual.expected.json, then run with {}=1 and accept --yes",
                    case.manifest.id, ACCEPT_ENVIRONMENT
                );
            } else {
                let options = resolve_live_options(&repository, &live_options)?;
                let run = run_live_case(case, &options)?;
                verify_expectations(&case.manifest.id, &case.manifest.expect, &run.observation)?;
                write_actual_recording(case, &run)?;
                println!(
                    "recorded\t{}\t{}\nreview actual.* files, then run with {}=1 and accept --yes",
                    case.manifest.id, run.invocation_id, ACCEPT_ENVIRONMENT
                );
            }
        }
        CaseCommand::Accept { case_id, yes } => {
            ensure!(yes, "accept requires --yes");
            ensure!(
                env::var(ACCEPT_ENVIRONMENT).as_deref() == Ok("1"),
                "accept requires {ACCEPT_ENVIRONMENT}=1"
            );
            let case = exact_case(&cases, &case_id)?;
            accept_actual_recording(case)?;
            verify_recorded(case)?;
            println!("accepted\t{}\tok", case.manifest.id);
        }
        CaseCommand::Schema => unreachable!(),
    }
    Ok(())
}

fn verify_recorded(case: &LoadedCase) -> Result<()> {
    verify_recorded_case(case)?;
    Ok(())
}

fn write_actual_recording(case: &LoadedCase, run: &bazel_mcp_reducer_cases::LiveRun) -> Result<()> {
    let canonical = case.manifest.evidence.clone().unwrap_or_default();
    let actual = actual_evidence(&canonical);
    let replacements = replacement_storage(run);
    let replacement_refs = replacements
        .iter()
        .map(|(label, source)| (label.as_str(), source.as_slice()))
        .collect::<Vec<_>>();

    write_sanitized(
        &run.invocation_paths.bep,
        &case.evidence_path(&actual.bep),
        true,
        &replacement_refs,
    )?;
    write_sanitized(
        &run.invocation_paths.stdout,
        &case.evidence_path(&actual.stdout),
        false,
        &replacement_refs,
    )?;
    write_sanitized(
        &run.invocation_paths.stderr,
        &case.evidence_path(&actual.stderr),
        false,
        &replacement_refs,
    )?;
    let exit_code = run
        .observation
        .exit_code
        .context("live result has no exit code to record")?;
    fs::write(case.evidence_path(&actual.exit), format!("{exit_code}\n"))?;

    if !canonical.test_logs.is_empty() {
        ensure!(
            run.invocation_paths.test_logs_raw.is_file(),
            "case expects test logs but the live invocation retained none"
        );
        let bytes = fs::read(&run.invocation_paths.test_logs_raw)?;
        let sanitized = sanitize_text(&bytes, &replacement_refs)?;
        for path in &actual.test_logs {
            fs::write(case.evidence_path(path), &sanitized)?;
        }
    }

    let replay = replay_with_evidence(case, &actual)?;
    let mut raw_text = String::new();
    for path in [&actual.stdout, &actual.stderr]
        .into_iter()
        .chain(actual.test_logs.iter())
    {
        raw_text.push_str(&String::from_utf8_lossy(&fs::read(
            case.evidence_path(path),
        )?));
    }
    let replay_observation = observe_replay(&replay, exit_code, raw_text);
    verify_expectations(
        &case.manifest.id,
        &case.manifest.expect,
        &replay_observation,
    )?;
    verify_live_replay_parity(&case.manifest.id, &run.observation, &replay_observation)?;
    fs::write(
        case.evidence_path(&actual.expected),
        serde_json::to_string_pretty(&replay)? + "\n",
    )?;
    let provenance = RecordedProvenance {
        schema_version: 1,
        case: &case.manifest.id,
        workspace: &case.manifest.workspace,
        command: &case.manifest.command,
        args: &case.manifest.args,
        bazel_version: &run.bazel_version,
        platform: env::consts::OS,
        architecture: env::consts::ARCH,
        tool: &case.manifest.provenance.tool,
        tool_version: &case.manifest.provenance.tool_version,
        rules: &case.manifest.provenance.rules,
        origin_repository: &case.manifest.provenance.origin_repository,
        origin_commit: &case.manifest.provenance.origin_commit,
    };
    fs::write(
        case.evidence_path(&actual.provenance),
        serde_json::to_string_pretty(&provenance)? + "\n",
    )?;
    Ok(())
}

fn accept_actual_recording(case: &LoadedCase) -> Result<()> {
    let canonical = case
        .manifest
        .evidence
        .as_ref()
        .context("accept requires an evidence declaration")?;
    let actual = actual_evidence(canonical);
    let existing = actual
        .paths()
        .into_iter()
        .filter(|(_, path)| case.evidence_path(path).is_file())
        .map(|(name, _)| name)
        .collect::<Vec<_>>();
    if existing == ["evidence.expected"] {
        let output = verify_case_contract(case, canonical)?;
        let expected_path = case.evidence_path(&actual.expected);
        let bytes = fs::read(&expected_path)?;
        verify_sanitized_evidence(&bytes)?;
        ensure!(
            bytes == (serde_json::to_string_pretty(&output)? + "\n").as_bytes(),
            "actual replay golden does not match the current deterministic replay"
        );
        let destination = case.evidence_path(&canonical.expected);
        if destination.is_file() {
            fs::remove_file(&destination)?;
        }
        fs::rename(&expected_path, destination)?;
        return Ok(());
    }
    verify_case_evidence(case, &actual)
        .context("recorded actual evidence did not pass replay validation")?;
    for ((actual_name, actual_path), (canonical_name, canonical_path)) in
        actual.paths().into_iter().zip(canonical.paths())
    {
        ensure!(
            actual_name == canonical_name,
            "internal evidence mapping mismatch: {actual_name} != {canonical_name}"
        );
        let source = case.evidence_path(actual_path);
        let destination = case.evidence_path(canonical_path);
        ensure!(
            source.is_file(),
            "missing recorded actual file {}",
            source.display()
        );
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        if destination.is_file() {
            fs::remove_file(&destination).with_context(|| {
                format!("remove prior accepted evidence {}", destination.display())
            })?;
        }
        fs::rename(&source, &destination)
            .with_context(|| format!("accept {} as {}", source.display(), destination.display()))?;
    }
    Ok(())
}

fn write_actual_replay(case: &LoadedCase) -> Result<()> {
    let canonical = case
        .manifest
        .evidence
        .as_ref()
        .context("replay recording requires an evidence declaration")?;
    let output = verify_case_contract(case, canonical)?;
    fs::write(
        case.evidence_path(&actual_evidence(canonical).expected),
        serde_json::to_string_pretty(&output)? + "\n",
    )?;
    Ok(())
}

fn actual_evidence(canonical: &EvidenceSpec) -> EvidenceSpec {
    EvidenceSpec {
        bep: "actual.events.bep".into(),
        stdout: "actual.stdout.txt".into(),
        stderr: "actual.stderr.txt".into(),
        exit: "actual.exit.code".into(),
        test_logs: canonical
            .test_logs
            .iter()
            .enumerate()
            .map(|(index, _)| format!("actual.test-log-{index}.txt").into())
            .collect(),
        expected: "actual.expected.json".into(),
        provenance: "actual.provenance.json".into(),
    }
}

fn replacement_storage(run: &bazel_mcp_reducer_cases::LiveRun) -> Vec<(String, Vec<u8>)> {
    let mut replacements = vec![
        (
            "WORKSPACE".to_owned(),
            run.workspace.to_string_lossy().as_bytes().to_vec(),
        ),
        (
            "CACHE_ROOT".to_owned(),
            run.cache_root.to_string_lossy().as_bytes().to_vec(),
        ),
        (
            "OUTPUT_ROOT".to_owned(),
            run.output_user_root.to_string_lossy().as_bytes().to_vec(),
        ),
        (
            "RUNTIME_ROOT".to_owned(),
            run.runtime_root().to_string_lossy().as_bytes().to_vec(),
        ),
    ];
    if let Ok(canonical) = run.runtime_root().canonicalize() {
        replacements.push((
            "RUNTIME_ROOT".to_owned(),
            canonical.to_string_lossy().as_bytes().to_vec(),
        ));
    }
    if let Some(home) = env::var_os("HOME") {
        replacements.push((
            "HOME".to_owned(),
            PathBuf::from(home).to_string_lossy().as_bytes().to_vec(),
        ));
    }
    replacements
}

fn write_sanitized(
    source: &Path,
    destination: &Path,
    binary: bool,
    replacements: &[(&str, &[u8])],
) -> Result<()> {
    let bytes =
        fs::read(source).with_context(|| format!("read retained evidence {}", source.display()))?;
    let sanitized = if binary {
        sanitize_binary(&bytes, replacements)?
    } else {
        sanitize_text(&bytes, replacements)?
    };
    fs::write(destination, sanitized)
        .with_context(|| format!("write recorded evidence {}", destination.display()))?;
    Ok(())
}

fn select_cases<'a>(
    repository: &Path,
    cases: &'a [LoadedCase],
    selection: &Selection,
) -> Result<Vec<&'a LoadedCase>> {
    let changed = selection
        .changed_from
        .as_deref()
        .map(|reference| changed_paths(repository, reference))
        .transpose()?;
    let shared_change = changed
        .as_ref()
        .is_some_and(|paths| paths.iter().any(|path| is_shared_change(path)));
    let selected = cases
        .iter()
        .filter(|case| {
            (selection.case_ids.is_empty() || selection.case_ids.contains(&case.manifest.id))
                && (selection.workspace.is_empty()
                    || selection.workspace.contains(&case.manifest.workspace))
                && (selection.tag.is_empty()
                    || selection
                        .tag
                        .iter()
                        .any(|tag| case.manifest.tags.contains(tag)))
                && changed.as_ref().is_none_or(|paths| {
                    (shared_change && case.manifest.tags.iter().any(|tag| tag == "live-smoke"))
                        || paths
                            .iter()
                            .any(|path| path_matches_case(repository, case, path))
                })
        })
        .collect::<Vec<_>>();
    ensure!(
        !selected.is_empty() || selection.allow_empty,
        "case selection matched no cases"
    );
    Ok(selected)
}

fn is_shared_change(path: &Path) -> bool {
    [
        "Cargo.lock",
        "Cargo.toml",
        "Makefile",
        "crates/bazel-mcp-reducer/",
        "crates/bazel-mcp-reducer-cases/",
        "crates/bazel-mcp-runner/",
        "crates/bazel-mcp-server/",
        "crates/bazel-mcp-store/",
        "crates/bazel-mcp-types/",
        "testdata/reducer-case.schema.json",
        "scripts/test-mcp-smoke.py",
        ".github/workflows/",
    ]
    .iter()
    .any(|prefix| path.to_string_lossy().starts_with(prefix))
}

fn path_matches_case(repository: &Path, case: &LoadedCase, changed: &Path) -> bool {
    let case_directory = case
        .manifest_path
        .parent()
        .unwrap_or(&case.manifest_path)
        .strip_prefix(repository)
        .unwrap_or_else(|_| case.manifest_path.parent().unwrap_or(&case.manifest_path));
    changed.starts_with(&case.manifest.workspace) || changed.starts_with(case_directory)
}

fn changed_paths(repository: &Path, reference: &str) -> Result<Vec<PathBuf>> {
    let output = Command::new("git")
        .args(["diff", "--name-only", &format!("{reference}...HEAD")])
        .current_dir(repository)
        .output()
        .context("run git diff for changed reducer cases")?;
    ensure!(
        output.status.success(),
        "git diff failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?
        .lines()
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect())
}

fn exact_case<'a>(cases: &'a [LoadedCase], id: &str) -> Result<&'a LoadedCase> {
    cases
        .iter()
        .find(|case| case.manifest.id == id)
        .with_context(|| format!("unknown reducer case {id:?}"))
}

fn resolve_live_options(repository: &Path, arguments: &LiveArguments) -> Result<LiveOptions> {
    let mut server = arguments
        .server
        .clone()
        .unwrap_or_else(|| repository.join("target/debug/bazel-mcp"));
    if cfg!(windows) && server.extension().is_none() {
        server.set_extension("exe");
    }
    ensure!(
        server.is_file(),
        "bazel-mcp server not found at {}; build it first or pass --server",
        server.display()
    );
    Ok(LiveOptions {
        repository_root: repository.to_owned(),
        server,
        bazel_executable: arguments.bazel.clone(),
        runtime_parent: arguments.runtime_parent.clone(),
        bazel_version: arguments.bazel_version.clone(),
    })
}

fn canonicalize(path: PathBuf) -> Result<PathBuf> {
    path.canonicalize()
        .with_context(|| format!("canonicalize {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bazel_mcp_reducer_cases::{
        AbsentExpectation, CaseExpectation, CaseManifest, ProvenanceSpec, ReplaySpec,
    };

    fn loaded_case(repository: &Path) -> LoadedCase {
        LoadedCase {
            manifest_path: repository.join("testdata/reducer-corpus/go/compiler/type/case.toml"),
            directory: repository.join("testdata/reducer-corpus/go/compiler/type"),
            manifest: CaseManifest {
                schema_version: 1,
                id: "go/compiler/type".to_owned(),
                issue: None,
                workspace: "examples/go".into(),
                command: "build".to_owned(),
                args: Vec::new(),
                startup_args: Vec::new(),
                tags: vec!["fast".to_owned()],
                platforms: vec!["any".to_owned()],
                timeout_seconds: 60,
                bazel_version: Some("9.2.0".to_owned()),
                test_target: None,
                evidence: None,
                replay: ReplaySpec::default(),
                expect: CaseExpectation {
                    state: "failed".to_owned(),
                    exit_code: Some(1),
                    headline_equals: None,
                    headline_contains: None,
                    inspect_hint: None,
                    max_visible_bytes: 8192,
                    max_diagnostics: 20,
                    diagnostics: Vec::new(),
                    artifacts: Vec::new(),
                    absent: AbsentExpectation::default(),
                },
                provenance: ProvenanceSpec {
                    tool: "go".to_owned(),
                    tool_version: "1".to_owned(),
                    rules: BTreeMap::new(),
                    origin_repository: None,
                    origin_commit: None,
                },
            },
        }
    }

    #[test]
    fn changed_paths_select_the_whole_case_directory_and_workspace() {
        let repository = Path::new("/repo");
        let case = loaded_case(repository);
        assert!(path_matches_case(
            repository,
            &case,
            Path::new("examples/go/cases/type_failure.go")
        ));
        assert!(path_matches_case(
            repository,
            &case,
            Path::new("testdata/reducer-corpus/go/compiler/type/expected.json")
        ));
        assert!(!path_matches_case(
            repository,
            &case,
            Path::new("examples/python/cases/failure.py")
        ));
    }

    #[test]
    fn shared_changes_are_explicit_and_do_not_capture_unrelated_docs() {
        assert!(is_shared_change(Path::new(
            "crates/bazel-mcp-reducer/src/build.rs"
        )));
        assert!(is_shared_change(Path::new(
            "testdata/reducer-case.schema.json"
        )));
        assert!(!is_shared_change(Path::new("docs/design.md")));
    }
}
