use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::Context;
use bazel_mcp_policy::PolicyConfig;
use bazel_mcp_runner::RunnerConfig;
use clap::Parser;
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};

#[cfg(test)]
use bazel_mcp_runner::BepTransport;

pub use bazel_mcp_policy::RawPolicyConfig;
pub use bazel_mcp_runner::{RawRunnerConfig, RawStarlarkConfig};
pub use bazel_mcp_store::{RawRetentionConfig, RetentionConfig};

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

/// Serializable MCP transport and task-lifecycle settings.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RawMcpConfig {
    pub progress_initial_seconds: u64,
    pub progress_interval_seconds: u64,
    pub result_encoding: ResultEncoding,
    pub mcp_execution_policy: McpExecutionPolicy,
    pub task_ttl_seconds: u64,
    pub task_poll_interval_ms: u64,
}

impl Default for RawMcpConfig {
    fn default() -> Self {
        Self {
            progress_initial_seconds: 30,
            progress_interval_seconds: 60,
            result_encoding: ResultEncoding::default(),
            mcp_execution_policy: McpExecutionPolicy::Auto,
            task_ttl_seconds: 24 * 60 * 60,
            task_poll_interval_ms: 2_000,
        }
    }
}

/// Flat TOML compatibility layer composed from subsystem-owned raw settings.
#[derive(Clone, Debug, Serialize)]
pub struct RawServerConfig {
    pub cache_root: PathBuf,
    #[serde(flatten)]
    pub policy: RawPolicyConfig,
    #[serde(flatten)]
    pub runner: RawRunnerConfig,
    #[serde(flatten)]
    pub retention: RawRetentionConfig,
    #[serde(flatten)]
    pub mcp: RawMcpConfig,
}

impl Default for RawServerConfig {
    fn default() -> Self {
        Self {
            cache_root: default_cache_root(),
            policy: RawPolicyConfig::default(),
            runner: RawRunnerConfig::default(),
            retention: RawRetentionConfig::default(),
            mcp: RawMcpConfig::default(),
        }
    }
}

#[derive(Deserialize)]
#[serde(default)]
struct RawServerConfigWire {
    cache_root: PathBuf,
    #[serde(flatten)]
    policy: RawPolicyConfig,
    #[serde(flatten)]
    runner: RawRunnerConfig,
    #[serde(flatten)]
    retention: RawRetentionConfig,
    #[serde(flatten)]
    mcp: RawMcpConfig,
    #[serde(flatten)]
    unknown: BTreeMap<String, toml::Value>,
}

impl Default for RawServerConfigWire {
    fn default() -> Self {
        let raw = RawServerConfig::default();
        Self {
            cache_root: raw.cache_root,
            policy: raw.policy,
            runner: raw.runner,
            retention: raw.retention,
            mcp: raw.mcp,
            unknown: BTreeMap::new(),
        }
    }
}

impl<'de> Deserialize<'de> for RawServerConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // Serde cannot combine `deny_unknown_fields` with flattened structs.
        // Collecting the unclaimed flat keys preserves the previous strict
        // compatibility surface without duplicating subsystem fields here.
        let raw = RawServerConfigWire::deserialize(deserializer)?;
        if let Some(field) = raw.unknown.keys().next() {
            return Err(D::Error::custom(format!("unknown field `{field}`")));
        }
        Ok(Self {
            cache_root: raw.cache_root,
            policy: raw.policy,
            runner: raw.runner,
            retention: raw.retention,
            mcp: raw.mcp,
        })
    }
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

impl TryFrom<RawMcpConfig> for McpConfig {
    type Error = anyhow::Error;

    fn try_from(raw: RawMcpConfig) -> Result<Self, Self::Error> {
        if !(100..=60_000).contains(&raw.task_poll_interval_ms) {
            anyhow::bail!("task poll interval must be between 100 and 60000 milliseconds");
        }
        Ok(Self {
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
        })
    }
}

/// Configuration that has passed path normalization and all subsystem checks.
#[derive(Clone, Debug)]
pub struct ValidatedServerConfig {
    cache_root: PathBuf,
    runner: RunnerConfig,
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
        raw.policy
            .allowed_roots
            .extend(cli.allowed_roots.iter().cloned());
        if let Some(cache_root) = &cli.cache_root {
            raw.cache_root = cache_root.clone();
        }
        Self::try_from_raw(raw, config_path.as_deref())
    }

    fn try_from_raw(mut raw: RawServerConfig, config_path: Option<&Path>) -> anyhow::Result<Self> {
        canonicalize_starlark_files(&mut raw.runner.starlark.files, config_path)?;
        raw.cache_root = canonicalize_with_missing_tail(&raw.cache_root)?;
        raw.policy.allowed_roots = raw
            .policy
            .allowed_roots
            .iter()
            .map(|root| {
                root.canonicalize()
                    .with_context(|| format!("canonicalize allowed root {}", root.display()))
            })
            .collect::<anyhow::Result<_>>()?;
        if let Some(output_user_root) = &raw.runner.output_user_root {
            raw.runner.output_user_root = Some(canonicalize_with_missing_tail(output_user_root)?);
        }
        if let Some(executable) = &raw.policy.bazel_executable {
            raw.policy.bazel_executable = Some(canonicalize_with_missing_tail(executable)?);
        }
        for root in &raw.policy.allowed_roots {
            if raw.cache_root.starts_with(root) {
                anyhow::bail!("cache root must not be inside an allowed Bazel workspace root");
            }
        }

        let policy = PolicyConfig::try_from(raw.policy).context("validate policy configuration")?;
        let runner = RunnerConfig::try_from((raw.runner, policy))
            .context("validate runner configuration")?;
        let retention =
            RetentionConfig::try_from(raw.retention).context("validate retention configuration")?;
        let mcp = McpConfig::try_from(raw.mcp).context("validate MCP configuration")?;
        Ok(Self {
            cache_root: raw.cache_root,
            runner,
            retention,
            mcp,
        })
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
        config.runner.policy.clone()
    }
}

impl From<ValidatedServerConfig> for PolicyConfig {
    fn from(config: ValidatedServerConfig) -> Self {
        Self::from(&config)
    }
}

impl From<&ValidatedServerConfig> for RunnerConfig {
    fn from(config: &ValidatedServerConfig) -> Self {
        config.runner.clone()
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

fn canonicalize_starlark_files(
    files: &mut Vec<PathBuf>,
    config_path: Option<&Path>,
) -> anyhow::Result<()> {
    let base = config_path
        .and_then(Path::parent)
        .map(Path::to_owned)
        .unwrap_or(std::env::current_dir()?);
    let mut seen = BTreeSet::new();
    *files = files
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

        assert!(config.runner.policy.allowed_roots.is_empty());
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
            RawServerConfig::default().mcp.result_encoding,
            ResultEncoding::Toon
        );
    }

    #[test]
    fn task_configuration_defaults_and_round_trips() {
        let config = RawServerConfig::default();
        assert_eq!(config.mcp.mcp_execution_policy, McpExecutionPolicy::Auto);
        assert_eq!(config.mcp.task_ttl_seconds, 86_400);
        assert_eq!(config.mcp.task_poll_interval_ms, 2_000);

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
        assert_eq!(
            RawServerConfig::default().runner.bep_transport,
            BepTransport::Tail
        );
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

    #[test]
    fn flat_toml_round_trips_through_subsystem_raw_configs() {
        let raw = RawServerConfig::default();
        let encoded = toml::to_string(&raw).unwrap();
        assert!(!encoded.contains("[policy]"));
        assert!(!encoded.contains("[runner]"));
        assert!(!encoded.contains("[retention]"));
        assert!(!encoded.contains("[mcp]"));
        assert!(encoded.contains("global_concurrency = 4"));
        assert!(encoded.contains("retention_days = 7"));
        assert!(encoded.contains("task_poll_interval_ms = 2000"));

        let decoded: RawServerConfig = toml::from_str(&encoded).unwrap();
        assert_eq!(decoded.policy, RawPolicyConfig::default());
        assert_eq!(decoded.runner, RawRunnerConfig::default());
        assert_eq!(decoded.retention, RawRetentionConfig::default());
        assert_eq!(decoded.mcp, RawMcpConfig::default());
    }
}
