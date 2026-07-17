use std::{
    collections::BTreeSet,
    ffi::OsString,
    num::{NonZeroU64, NonZeroUsize},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::Context;
use bazel_mcp_policy::PolicyConfig;
use bazel_mcp_runner::{BepTransport, RunnerConfig, StarlarkLimits, StarlarkReducerConfig};
use clap::Parser;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultEncoding {
    Text,
    #[default]
    Toon,
    Structured,
    Both,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpExecutionPolicy {
    #[default]
    Auto,
    SyncOnly,
    TasksRequired,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RawStarlarkConfig {
    pub files: Vec<PathBuf>,
    pub max_source_bytes: usize,
    pub max_input_bytes: usize,
    pub max_events: usize,
    pub max_output_bytes: usize,
    pub max_output_items: usize,
    pub max_ticks: u64,
    pub max_heap_bytes: usize,
    pub max_callstack_size: usize,
    pub timeout_ms: u64,
}

impl Default for RawStarlarkConfig {
    fn default() -> Self {
        let limits = StarlarkLimits::default();
        Self {
            files: Vec::new(),
            max_source_bytes: limits.max_source_bytes,
            max_input_bytes: limits.max_input_bytes,
            max_events: limits.max_events,
            max_output_bytes: limits.max_output_bytes,
            max_output_items: limits.max_output_items,
            max_ticks: limits.max_ticks,
            max_heap_bytes: limits.max_heap_bytes,
            max_callstack_size: limits.max_callstack_size,
            timeout_ms: u64::try_from(limits.timeout.as_millis()).unwrap_or(100),
        }
    }
}

impl RawStarlarkConfig {
    #[must_use]
    pub fn runner_config(&self) -> StarlarkReducerConfig {
        StarlarkReducerConfig {
            files: self.files.clone(),
            limits: StarlarkLimits {
                max_source_bytes: self.max_source_bytes,
                max_input_bytes: self.max_input_bytes,
                max_events: self.max_events,
                max_output_bytes: self.max_output_bytes,
                max_output_items: self.max_output_items,
                max_ticks: self.max_ticks,
                max_heap_bytes: self.max_heap_bytes,
                max_callstack_size: self.max_callstack_size,
                timeout: std::time::Duration::from_millis(self.timeout_ms),
            },
        }
    }

    fn canonicalize_files(&mut self, config_path: Option<&Path>) -> anyhow::Result<()> {
        if self.max_source_bytes == 0
            || self.max_input_bytes == 0
            || self.max_events == 0
            || self.max_output_bytes == 0
            || self.max_output_items == 0
            || self.max_ticks == 0
            || self.max_heap_bytes == 0
            || self.max_callstack_size == 0
            || self.timeout_ms == 0
        {
            anyhow::bail!("all Starlark reducer resource limits must be greater than zero");
        }
        let base = config_path
            .and_then(Path::parent)
            .map(Path::to_owned)
            .unwrap_or(std::env::current_dir()?);
        let mut seen = BTreeSet::new();
        self.files = self
            .files
            .iter()
            .map(|path| {
                let path = if path.is_absolute() {
                    path.clone()
                } else {
                    base.join(path)
                };
                let path = path
                    .canonicalize()
                    .with_context(|| format!("canonicalize Starlark reducer {}", path.display()))?;
                if !path.is_file() {
                    anyhow::bail!("Starlark reducer is not a regular file: {}", path.display());
                }
                if !seen.insert(path.clone()) {
                    anyhow::bail!("duplicate Starlark reducer file: {}", path.display());
                }
                Ok(path)
            })
            .collect::<anyhow::Result<_>>()?;
        Ok(())
    }
}

#[derive(Clone, Debug, Parser)]
#[command(name = "bazel-mcp", version, about)]
pub struct Cli {
    /// Read configuration from this TOML file.
    #[arg(long, env = "BAZEL_MCP_CONFIG")]
    pub config: Option<PathBuf>,
    /// Add an allowed workspace root. May be repeated.
    #[arg(long = "allow-root")]
    pub allowed_roots: Vec<PathBuf>,
    /// Override the invocation cache directory.
    #[arg(long)]
    pub cache_root: Option<PathBuf>,
    /// Tracing filter written to stderr (stdout is MCP-only).
    #[arg(long, default_value = "bazel_mcp=info")]
    pub log: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RawServerConfig {
    pub allowed_roots: Vec<PathBuf>,
    pub cache_root: PathBuf,
    pub bazel_executable: Option<PathBuf>,
    pub output_user_root: Option<PathBuf>,
    pub allowed_commands: BTreeSet<String>,
    pub denied_commands: BTreeSet<String>,
    pub environment_allowlist: BTreeSet<String>,
    pub redaction_patterns: Vec<String>,
    pub global_concurrency: usize,
    pub default_timeout_seconds: u64,
    pub maximum_timeout_seconds: u64,
    pub cancellation_interrupt_grace_seconds: u64,
    pub cancellation_terminate_grace_seconds: u64,
    pub progress_initial_seconds: u64,
    pub progress_interval_seconds: u64,
    pub retention_days: u64,
    pub maximum_storage_bytes: u64,
    pub result_encoding: ResultEncoding,
    pub supported_bazel_major_versions: BTreeSet<u32>,
    pub allow_unsupported_bazel_versions: bool,
    pub version_check_timeout_seconds: u64,
    pub maximum_pending_invocations: usize,
    pub retention_cleanup_interval_seconds: u64,
    pub isolated_bazel_server_idle_seconds: u64,
    pub mcp_execution_policy: McpExecutionPolicy,
    pub task_ttl_seconds: u64,
    pub task_poll_interval_ms: u64,
    pub bep_transport: BepTransport,
    pub starlark: RawStarlarkConfig,
}

impl Default for RawServerConfig {
    fn default() -> Self {
        let policy = bazel_mcp_policy::PolicyConfig::default();
        Self {
            allowed_roots: Vec::new(),
            cache_root: default_cache_root(),
            bazel_executable: None,
            output_user_root: None,
            allowed_commands: policy.allowed_commands,
            denied_commands: policy.denied_commands,
            environment_allowlist: policy.environment_allowlist,
            redaction_patterns: Vec::new(),
            global_concurrency: 4,
            default_timeout_seconds: 30 * 60,
            maximum_timeout_seconds: 2 * 60 * 60,
            cancellation_interrupt_grace_seconds: 10,
            cancellation_terminate_grace_seconds: 5,
            progress_initial_seconds: 30,
            progress_interval_seconds: 60,
            retention_days: 7,
            maximum_storage_bytes: 10 * 1024 * 1024 * 1024,
            result_encoding: ResultEncoding::default(),
            supported_bazel_major_versions: [8, 9].into_iter().collect(),
            allow_unsupported_bazel_versions: false,
            version_check_timeout_seconds: 30,
            maximum_pending_invocations: 256,
            retention_cleanup_interval_seconds: 60 * 60,
            isolated_bazel_server_idle_seconds: 60,
            mcp_execution_policy: McpExecutionPolicy::Auto,
            task_ttl_seconds: 24 * 60 * 60,
            task_poll_interval_ms: 2_000,
            bep_transport: BepTransport::Tail,
            starlark: RawStarlarkConfig::default(),
        }
    }
}

#[derive(Clone, Debug)]
struct ValidatedRunnerConfig {
    default_timeout: Duration,
    maximum_timeout: Duration,
    cancellation_interrupt_grace: Duration,
    cancellation_terminate_grace: Duration,
    global_concurrency: NonZeroUsize,
    output_user_root: Option<PathBuf>,
    isolated_bazel_server_idle_timeout: Duration,
    supported_bazel_major_versions: BTreeSet<u32>,
    allow_unsupported_bazel_versions: bool,
    version_check_timeout: Duration,
    maximum_pending_invocations: NonZeroUsize,
    output_base_lock_root: PathBuf,
    bep_transport: BepTransport,
    starlark_reducers: StarlarkReducerConfig,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetentionConfig {
    pub maximum_age: Duration,
    pub maximum_storage_bytes: NonZeroU64,
    pub cleanup_interval: Duration,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpConfig {
    pub result_encoding: ResultEncoding,
    pub progress_initial: Duration,
    pub progress_interval: Duration,
    pub execution_policy: McpExecutionPolicy,
    pub task_ttl: Duration,
    pub task_poll_interval: Duration,
}

/// Configuration that has passed path normalization and all subsystem checks.
#[derive(Clone, Debug)]
pub struct ValidatedServerConfig {
    cache_root: PathBuf,
    policy: PolicyConfig,
    runner: ValidatedRunnerConfig,
    retention: RetentionConfig,
    mcp: McpConfig,
}

impl ValidatedServerConfig {
    pub fn load(cli: &Cli) -> anyhow::Result<Self> {
        let config_path = cli.config.clone().or_else(default_config_path_if_present);
        let mut raw: RawServerConfig = if let Some(path) = &config_path {
            let source = std::fs::read_to_string(path)
                .with_context(|| format!("read configuration {}", path.display()))?;
            toml::from_str(&source)
                .with_context(|| format!("parse configuration {}", path.display()))?
        } else {
            RawServerConfig::default()
        };
        raw.allowed_roots.extend(cli.allowed_roots.iter().cloned());
        if let Some(cache_root) = &cli.cache_root {
            raw.cache_root = cache_root.clone();
        }
        Self::try_from_raw(raw, config_path.as_deref())
    }

    fn try_from_raw(mut raw: RawServerConfig, config_path: Option<&Path>) -> anyhow::Result<Self> {
        if !(100..=60_000).contains(&raw.task_poll_interval_ms) {
            anyhow::bail!("task poll interval must be between 100 and 60000 milliseconds");
        }
        raw.starlark.canonicalize_files(config_path)?;
        raw.cache_root = canonicalize_with_missing_tail(&raw.cache_root)?;
        raw.allowed_roots = raw
            .allowed_roots
            .iter()
            .map(|root| {
                root.canonicalize()
                    .with_context(|| format!("canonicalize allowed root {}", root.display()))
            })
            .collect::<anyhow::Result<_>>()?;
        if let Some(output_user_root) = &raw.output_user_root {
            raw.output_user_root = Some(canonicalize_with_missing_tail(output_user_root)?);
        }
        if let Some(executable) = &raw.bazel_executable {
            raw.bazel_executable = Some(canonicalize_with_missing_tail(executable)?);
        }
        for root in &raw.allowed_roots {
            if raw.cache_root.starts_with(root) {
                anyhow::bail!("cache root must not be inside an allowed Bazel workspace root");
            }
        }

        let global_concurrency = NonZeroUsize::new(raw.global_concurrency)
            .context("global concurrency must be greater than zero")?;
        let maximum_pending_invocations = NonZeroUsize::new(raw.maximum_pending_invocations)
            .context("maximum pending invocations must be greater than zero")?;
        let maximum_storage_bytes = NonZeroU64::new(raw.maximum_storage_bytes)
            .context("maximum storage bytes must be greater than zero")?;

        let retention = RetentionConfig {
            maximum_age: Duration::from_secs(raw.retention_days.saturating_mul(24 * 60 * 60)),
            maximum_storage_bytes,
            cleanup_interval: nonzero_duration(
                raw.retention_cleanup_interval_seconds,
                "retention cleanup interval must be greater than zero",
            )?,
        };
        let mcp = McpConfig {
            result_encoding: raw.result_encoding,
            progress_initial: nonzero_duration(
                raw.progress_initial_seconds,
                "progress timing must be greater than zero",
            )?,
            progress_interval: nonzero_duration(
                raw.progress_interval_seconds,
                "progress timing must be greater than zero",
            )?,
            execution_policy: raw.mcp_execution_policy,
            task_ttl: nonzero_duration(raw.task_ttl_seconds, "task TTL must be greater than zero")?,
            task_poll_interval: Duration::from_millis(raw.task_poll_interval_ms),
        };
        let policy = PolicyConfig {
            allowed_roots: raw.allowed_roots,
            allowed_commands: raw.allowed_commands,
            denied_commands: raw.denied_commands,
            environment_allowlist: raw.environment_allowlist,
            redaction_patterns: raw.redaction_patterns,
            bazel_executable: raw.bazel_executable,
        };
        let runner = ValidatedRunnerConfig {
            default_timeout: Duration::from_secs(raw.default_timeout_seconds),
            maximum_timeout: Duration::from_secs(raw.maximum_timeout_seconds),
            cancellation_interrupt_grace: Duration::from_secs(
                raw.cancellation_interrupt_grace_seconds,
            ),
            cancellation_terminate_grace: Duration::from_secs(
                raw.cancellation_terminate_grace_seconds,
            ),
            global_concurrency,
            output_user_root: raw.output_user_root,
            isolated_bazel_server_idle_timeout: Duration::from_secs(
                raw.isolated_bazel_server_idle_seconds,
            ),
            supported_bazel_major_versions: raw.supported_bazel_major_versions,
            allow_unsupported_bazel_versions: raw.allow_unsupported_bazel_versions,
            version_check_timeout: Duration::from_secs(raw.version_check_timeout_seconds),
            maximum_pending_invocations,
            output_base_lock_root: RunnerConfig::default().output_base_lock_root,
            bep_transport: raw.bep_transport,
            starlark_reducers: raw.starlark.runner_config(),
        };
        let config = Self {
            cache_root: raw.cache_root,
            policy,
            runner,
            retention,
            mcp,
        };
        RunnerConfig::from(&config)
            .validate()
            .context("validate runner configuration")?;
        Ok(config)
    }

    #[must_use]
    pub fn clamp_timeout(&self, requested: Option<u64>) -> u64 {
        requested
            .unwrap_or(self.runner.default_timeout.as_secs())
            .clamp(1, self.runner.maximum_timeout.as_secs())
    }

    #[must_use]
    pub fn cache_root(&self) -> &Path {
        &self.cache_root
    }

    #[must_use]
    pub fn shutdown_wait(&self) -> Duration {
        self.runner
            .cancellation_interrupt_grace
            .saturating_add(self.runner.cancellation_terminate_grace)
            .saturating_add(Duration::from_secs(5))
    }
}

impl TryFrom<RawServerConfig> for ValidatedServerConfig {
    type Error = anyhow::Error;

    fn try_from(raw: RawServerConfig) -> Result<Self, Self::Error> {
        Self::try_from_raw(raw, None)
    }
}

impl From<&ValidatedServerConfig> for PolicyConfig {
    fn from(config: &ValidatedServerConfig) -> Self {
        config.policy.clone()
    }
}

impl From<ValidatedServerConfig> for PolicyConfig {
    fn from(config: ValidatedServerConfig) -> Self {
        Self::from(&config)
    }
}

impl From<&ValidatedServerConfig> for RunnerConfig {
    fn from(config: &ValidatedServerConfig) -> Self {
        let runner = &config.runner;
        Self {
            policy: PolicyConfig::from(config),
            default_timeout: runner.default_timeout,
            maximum_timeout: runner.maximum_timeout,
            cancellation_interrupt_grace: runner.cancellation_interrupt_grace,
            cancellation_terminate_grace: runner.cancellation_terminate_grace,
            global_concurrency: runner.global_concurrency.get(),
            output_user_root: runner.output_user_root.clone(),
            isolated_bazel_server_idle_timeout: runner.isolated_bazel_server_idle_timeout,
            supported_bazel_major_versions: runner.supported_bazel_major_versions.clone(),
            allow_unsupported_bazel_versions: runner.allow_unsupported_bazel_versions,
            version_check_timeout: runner.version_check_timeout,
            maximum_pending_invocations: runner.maximum_pending_invocations.get(),
            output_base_lock_root: runner.output_base_lock_root.clone(),
            bep_transport: runner.bep_transport,
            starlark_reducers: runner.starlark_reducers.clone(),
        }
    }
}

impl From<ValidatedServerConfig> for RunnerConfig {
    fn from(config: ValidatedServerConfig) -> Self {
        Self::from(&config)
    }
}

impl From<&ValidatedServerConfig> for RetentionConfig {
    fn from(config: &ValidatedServerConfig) -> Self {
        config.retention.clone()
    }
}

impl From<ValidatedServerConfig> for RetentionConfig {
    fn from(config: ValidatedServerConfig) -> Self {
        Self::from(&config)
    }
}

impl From<&ValidatedServerConfig> for McpConfig {
    fn from(config: &ValidatedServerConfig) -> Self {
        config.mcp.clone()
    }
}

impl From<ValidatedServerConfig> for McpConfig {
    fn from(config: ValidatedServerConfig) -> Self {
        Self::from(&config)
    }
}

fn nonzero_duration(seconds: u64, message: &'static str) -> anyhow::Result<Duration> {
    if seconds == 0 {
        anyhow::bail!(message);
    }
    Ok(Duration::from_secs(seconds))
}

fn absolute_path(path: &Path) -> anyhow::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_owned())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn canonicalize_with_missing_tail(path: &Path) -> anyhow::Result<PathBuf> {
    let normalized = normalize_absolute(&absolute_path(path)?);
    let mut existing = normalized.as_path();
    let mut missing = Vec::<OsString>::new();
    loop {
        match std::fs::symlink_metadata(existing) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = existing.file_name().ok_or_else(|| {
                    anyhow::anyhow!("{} has no existing ancestor", normalized.display())
                })?;
                missing.push(name.to_owned());
                existing = existing.parent().ok_or_else(|| {
                    anyhow::anyhow!("{} has no existing ancestor", normalized.display())
                })?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    let mut canonical = existing
        .canonicalize()
        .with_context(|| format!("canonicalize path ancestor {}", existing.display()))?;
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

fn default_cache_root() -> PathBuf {
    if let Some(root) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(root).join("bazel-mcp");
    }
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
    if cfg!(target_os = "macos") {
        home.join("Library/Caches/bazel-mcp")
    } else {
        home.join(".cache/bazel-mcp")
    }
}

fn default_config_path_if_present() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let path = if let Some(root) = std::env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(root).join("bazel-mcp/config.toml")
    } else {
        home.join(".config/bazel-mcp/config.toml")
    };
    Path::new(&path).is_file().then_some(path)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    fn cli(config: PathBuf) -> Cli {
        Cli {
            config: Some(config),
            allowed_roots: Vec::new(),
            cache_root: None,
            log: "bazel_mcp=info".to_owned(),
        }
    }

    fn write_config(root: &Path, workspace: &Path, extra: &str) -> PathBuf {
        let path = root.join("config.toml");
        fs::write(
            &path,
            format!(
                "allowed_roots = [{workspace:?}]\ncache_root = {:?}\n{extra}",
                root.join("cache")
            ),
        )
        .unwrap();
        path
    }

    #[test]
    fn rejects_zero_concurrency_and_zero_maximum_timeout() {
        let root = tempdir().unwrap();
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).unwrap();

        let concurrency = write_config(root.path(), &workspace, "global_concurrency = 0\n");
        assert!(ValidatedServerConfig::load(&cli(concurrency)).is_err());

        let timeout = write_config(
            root.path(),
            &workspace,
            "default_timeout_seconds = 0\nmaximum_timeout_seconds = 0\n",
        );
        assert!(ValidatedServerConfig::load(&cli(timeout)).is_err());
    }

    #[test]
    fn accepts_configuration_without_allowed_roots() {
        let root = tempdir().unwrap();
        let path = root.path().join("config.toml");
        fs::write(
            &path,
            format!("cache_root = {:?}\n", root.path().join("cache")),
        )
        .unwrap();

        let config = ValidatedServerConfig::load(&cli(path)).unwrap();

        assert!(config.policy.allowed_roots.is_empty());
    }

    #[test]
    fn resolves_starlark_reducers_relative_to_the_configuration_file() {
        let root = tempdir().unwrap();
        let reducer = root.path().join("custom.star");
        fs::write(
            &reducer,
            "API_VERSION = 1\nNAME = \"custom\"\ndef reduce(ctx): return None\n",
        )
        .unwrap();
        let path = root.path().join("config.toml");
        fs::write(
            &path,
            format!(
                "cache_root = {:?}\n[starlark]\nfiles = [\"custom.star\"]\n",
                root.path().join("cache")
            ),
        )
        .unwrap();

        let config = ValidatedServerConfig::load(&cli(path)).unwrap();

        assert_eq!(
            config.runner.starlark_reducers.files,
            vec![reducer.canonicalize().unwrap()]
        );
        assert_eq!(
            config.runner.starlark_reducers.limits.timeout,
            Duration::from_millis(100)
        );
    }

    #[test]
    fn rejects_zero_starlark_resource_limits() {
        let root = tempdir().unwrap();
        let path = root.path().join("config.toml");
        fs::write(
            &path,
            format!(
                "cache_root = {:?}\n[starlark]\nmax_ticks = 0\n",
                root.path().join("cache")
            ),
        )
        .unwrap();

        assert!(ValidatedServerConfig::load(&cli(path)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn detects_a_cache_symlink_that_enters_an_allowed_root() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let cache_link = root.path().join("cache-link");
        symlink(&workspace, &cache_link).unwrap();
        let path = root.path().join("config.toml");
        fs::write(
            &path,
            format!("allowed_roots = [{workspace:?}]\ncache_root = {cache_link:?}\n"),
        )
        .unwrap();

        let error = ValidatedServerConfig::load(&cli(path)).unwrap_err();
        assert!(error.to_string().contains("cache root must not be inside"));
    }

    #[test]
    fn normalizes_missing_path_components_before_containment_checks() {
        let root = tempdir().unwrap();
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let path = root.path().join("config.toml");
        let cache = workspace.join("missing/../cache");
        fs::write(
            &path,
            format!("allowed_roots = [{workspace:?}]\ncache_root = {cache:?}\n"),
        )
        .unwrap();

        assert!(ValidatedServerConfig::load(&cli(path)).is_err());
    }

    #[test]
    fn loads_each_configured_result_encoding() {
        let root = tempdir().unwrap();
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).unwrap();

        for (name, expected) in [
            ("text", ResultEncoding::Text),
            ("toon", ResultEncoding::Toon),
            ("structured", ResultEncoding::Structured),
            ("both", ResultEncoding::Both),
        ] {
            let path = write_config(
                root.path(),
                &workspace,
                &format!("result_encoding = {name:?}\n"),
            );
            let config = ValidatedServerConfig::load(&cli(path)).unwrap();
            assert_eq!(config.mcp.result_encoding, expected);
        }
    }

    #[test]
    fn toon_is_the_default_result_encoding() {
        assert_eq!(ResultEncoding::default(), ResultEncoding::Toon);
        assert_eq!(
            RawServerConfig::default().result_encoding,
            ResultEncoding::Toon
        );
    }

    #[test]
    fn task_configuration_defaults_and_round_trips() {
        let config = RawServerConfig::default();
        assert_eq!(config.mcp_execution_policy, McpExecutionPolicy::Auto);
        assert_eq!(config.task_ttl_seconds, 86_400);
        assert_eq!(config.task_poll_interval_ms, 2_000);

        for (name, expected) in [
            ("auto", McpExecutionPolicy::Auto),
            ("sync_only", McpExecutionPolicy::SyncOnly),
            ("tasks_required", McpExecutionPolicy::TasksRequired),
        ] {
            let encoded = serde_json::to_string(&expected).unwrap();
            let decoded: McpExecutionPolicy = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, expected, "policy {name}");
        }
        assert!(serde_json::from_str::<McpExecutionPolicy>("\"detached\"").is_err());
    }

    #[test]
    fn bep_transport_defaults_to_tail_and_accepts_fifo_and_bes() {
        assert_eq!(RawServerConfig::default().bep_transport, BepTransport::Tail);
        assert_eq!(
            serde_json::from_str::<BepTransport>("\"fifo\"").unwrap(),
            BepTransport::Fifo
        );
        assert_eq!(
            serde_json::from_str::<BepTransport>("\"bes\"").unwrap(),
            BepTransport::Bes
        );
        assert!(serde_json::from_str::<BepTransport>("\"remote\"").is_err());
    }

    #[test]
    fn validates_task_lifecycle_settings_for_every_policy() {
        let root = tempdir().unwrap();
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).unwrap();

        for extra in [
            "task_ttl_seconds = 0\n",
            "task_poll_interval_ms = 99\n",
            "task_poll_interval_ms = 60001\n",
        ] {
            let path = write_config(root.path(), &workspace, extra);
            assert!(
                ValidatedServerConfig::load(&cli(path)).is_err(),
                "accepted {extra}"
            );
        }

        for (name, expected) in [
            ("auto", McpExecutionPolicy::Auto),
            ("sync_only", McpExecutionPolicy::SyncOnly),
            ("tasks_required", McpExecutionPolicy::TasksRequired),
        ] {
            let path = write_config(
                root.path(),
                &workspace,
                &format!(
                    "mcp_execution_policy = {name:?}\ntask_ttl_seconds = 1\ntask_poll_interval_ms = 100\n"
                ),
            );
            let config = ValidatedServerConfig::load(&cli(path)).unwrap();
            assert_eq!(config.mcp.execution_policy, expected);
        }
    }

    #[test]
    fn rejects_unknown_top_level_and_starlark_settings() {
        let root = tempdir().unwrap();
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).unwrap();

        let top_level = write_config(root.path(), &workspace, "concurency = 4\n");
        let error = ValidatedServerConfig::load(&cli(top_level)).unwrap_err();
        assert!(format!("{error:#}").contains("unknown field `concurency`"));

        let starlark = write_config(
            root.path(),
            &workspace,
            "[starlark]\nmax_tick_count = 100\n",
        );
        let error = ValidatedServerConfig::load(&cli(starlark)).unwrap_err();
        assert!(format!("{error:#}").contains("unknown field `max_tick_count`"));
    }

    #[test]
    fn projects_validated_typed_subsystem_configuration() {
        let root = tempdir().unwrap();
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let path = write_config(
            root.path(),
            &workspace,
            concat!(
                "global_concurrency = 3\n",
                "maximum_pending_invocations = 5\n",
                "default_timeout_seconds = 7\n",
                "maximum_timeout_seconds = 9\n",
                "retention_days = 2\n",
                "maximum_storage_bytes = 123\n",
                "retention_cleanup_interval_seconds = 11\n",
                "progress_initial_seconds = 12\n",
                "progress_interval_seconds = 13\n",
                "task_ttl_seconds = 14\n",
                "task_poll_interval_ms = 150\n",
            ),
        );

        let config = ValidatedServerConfig::load(&cli(path)).unwrap();
        let policy = PolicyConfig::from(&config);
        let runner = RunnerConfig::from(&config);
        let retention = RetentionConfig::from(&config);
        let mcp = McpConfig::from(&config);

        assert_eq!(
            policy.allowed_roots,
            vec![workspace.canonicalize().unwrap()]
        );
        assert_eq!(runner.global_concurrency, 3);
        assert_eq!(runner.maximum_pending_invocations, 5);
        assert_eq!(runner.default_timeout, Duration::from_secs(7));
        assert_eq!(runner.maximum_timeout, Duration::from_secs(9));
        assert_eq!(retention.maximum_age, Duration::from_secs(2 * 24 * 60 * 60));
        assert_eq!(retention.maximum_storage_bytes.get(), 123);
        assert_eq!(retention.cleanup_interval, Duration::from_secs(11));
        assert_eq!(mcp.progress_initial, Duration::from_secs(12));
        assert_eq!(mcp.progress_interval, Duration::from_secs(13));
        assert_eq!(mcp.task_ttl, Duration::from_secs(14));
        assert_eq!(mcp.task_poll_interval, Duration::from_millis(150));
    }
}
