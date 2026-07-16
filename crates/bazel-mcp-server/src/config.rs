use std::{
    collections::BTreeSet,
    ffi::OsString,
    path::{Path, PathBuf},
};

use anyhow::Context;
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
#[serde(default)]
pub struct ServerConfig {
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
}

impl Default for ServerConfig {
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
        }
    }
}

impl ServerConfig {
    pub fn load(cli: &Cli) -> anyhow::Result<Self> {
        let config_path = cli.config.clone().or_else(default_config_path_if_present);
        let mut config = if let Some(path) = config_path {
            let source = std::fs::read_to_string(&path)
                .with_context(|| format!("read configuration {}", path.display()))?;
            toml::from_str(&source)
                .with_context(|| format!("parse configuration {}", path.display()))?
        } else {
            Self::default()
        };
        config
            .allowed_roots
            .extend(cli.allowed_roots.iter().cloned());
        if let Some(cache_root) = &cli.cache_root {
            config.cache_root = cache_root.clone();
        }
        if config.global_concurrency == 0 {
            anyhow::bail!("global concurrency must be greater than zero");
        }
        if config.maximum_pending_invocations < config.global_concurrency {
            anyhow::bail!("maximum pending invocations must be at least global concurrency");
        }
        if config.maximum_timeout_seconds == 0 {
            anyhow::bail!("maximum timeout must be greater than zero");
        }
        if config.default_timeout_seconds > config.maximum_timeout_seconds {
            anyhow::bail!("default timeout exceeds maximum timeout");
        }
        if config.maximum_storage_bytes == 0 {
            anyhow::bail!("maximum storage bytes must be greater than zero");
        }
        if config.progress_initial_seconds == 0 || config.progress_interval_seconds == 0 {
            anyhow::bail!("progress timing must be greater than zero");
        }
        if config.version_check_timeout_seconds == 0 {
            anyhow::bail!("version check timeout must be greater than zero");
        }
        if config.retention_cleanup_interval_seconds == 0 {
            anyhow::bail!("retention cleanup interval must be greater than zero");
        }
        if config.isolated_bazel_server_idle_seconds == 0 {
            anyhow::bail!("isolated Bazel server idle timeout must be greater than zero");
        }
        if config.task_ttl_seconds == 0 {
            anyhow::bail!("task TTL must be greater than zero");
        }
        if !(100..=60_000).contains(&config.task_poll_interval_ms) {
            anyhow::bail!("task poll interval must be between 100 and 60000 milliseconds");
        }
        if !config.allow_unsupported_bazel_versions
            && config.supported_bazel_major_versions.is_empty()
        {
            anyhow::bail!("supported Bazel major versions must not be empty");
        }
        config.cache_root = canonicalize_with_missing_tail(&config.cache_root)?;
        config.allowed_roots = config
            .allowed_roots
            .iter()
            .map(|root| {
                root.canonicalize()
                    .with_context(|| format!("canonicalize allowed root {}", root.display()))
            })
            .collect::<anyhow::Result<_>>()?;
        if let Some(output_user_root) = &config.output_user_root {
            config.output_user_root = Some(canonicalize_with_missing_tail(output_user_root)?);
        }
        if let Some(executable) = &config.bazel_executable {
            config.bazel_executable = Some(canonicalize_with_missing_tail(executable)?);
        }
        for root in &config.allowed_roots {
            let cache = &config.cache_root;
            if cache.starts_with(root) {
                anyhow::bail!("cache root must not be inside an allowed Bazel workspace root");
            }
        }
        Ok(config)
    }

    #[must_use]
    pub fn clamp_timeout(&self, requested: Option<u64>) -> u64 {
        requested
            .unwrap_or(self.default_timeout_seconds)
            .clamp(1, self.maximum_timeout_seconds)
    }
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
        assert!(ServerConfig::load(&cli(concurrency)).is_err());

        let timeout = write_config(
            root.path(),
            &workspace,
            "default_timeout_seconds = 0\nmaximum_timeout_seconds = 0\n",
        );
        assert!(ServerConfig::load(&cli(timeout)).is_err());
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

        let config = ServerConfig::load(&cli(path)).unwrap();

        assert!(config.allowed_roots.is_empty());
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

        let error = ServerConfig::load(&cli(path)).unwrap_err();
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

        assert!(ServerConfig::load(&cli(path)).is_err());
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
            let config = ServerConfig::load(&cli(path)).unwrap();
            assert_eq!(config.result_encoding, expected);
        }
    }

    #[test]
    fn toon_is_the_default_result_encoding() {
        assert_eq!(ResultEncoding::default(), ResultEncoding::Toon);
        assert_eq!(
            ServerConfig::default().result_encoding,
            ResultEncoding::Toon
        );
    }

    #[test]
    fn task_configuration_defaults_and_round_trips() {
        let config = ServerConfig::default();
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
            assert!(ServerConfig::load(&cli(path)).is_err(), "accepted {extra}");
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
            let config = ServerConfig::load(&cli(path)).unwrap();
            assert_eq!(config.mcp_execution_policy, expected);
        }
    }
}
