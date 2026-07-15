# Configuration

`bazel-mcp` works without a configuration file. The built-in defaults allow
any Bazel workspace available to the current user and retain invocation
evidence in a local cache.

For shared environments or tighter local controls, configure allowed workspace
roots, command policy, retention, timeouts, environment variables, and
redaction explicitly.

## Loading configuration

The server looks for configuration in this order:

1. the file passed with `--config`;
2. the file named by `BAZEL_MCP_CONFIG`;
3. `$XDG_CONFIG_HOME/bazel-mcp/config.toml`, or
   `~/.config/bazel-mcp/config.toml` when `XDG_CONFIG_HOME` is not set;
4. built-in defaults.

The default path is used only when the file already exists. Command-line
`--allow-root` values are added to roots read from the file, and `--cache-root`
overrides the configured cache directory.

Start with the repository's [example configuration](../examples/config.toml):

```sh
mkdir -p ~/.config/bazel-mcp
cp examples/config.toml ~/.config/bazel-mcp/config.toml
```

Replace the example workspace path before starting the server.

## Common settings

### Restrict workspace access

An empty `allowed_roots` list permits every workspace available to the current
user. Production or shared configurations should list one or more absolute
roots:

```toml
allowed_roots = ["/work/company", "/work/open-source"]
```

A requested workspace must be contained by one of these roots. The invocation
cache cannot be located inside an allowed workspace root.

For a single root, the equivalent CLI option is:

```sh
bazel-mcp --allow-root /work/company
```

### Select Bazel

By default, `bazel-mcp` looks for an executable in this order:

1. `tools/bazel` in the requested workspace;
2. `bazelisk` on `PATH`;
3. `bazel` on `PATH`.

Set an explicit executable to bypass discovery:

```toml
bazel_executable = "/usr/local/bin/bazelisk"
```

### Pass environment variables

Child Bazel processes always receive `HOME`, `PATH`, `TMPDIR`, `TEMP`, `TMP`,
and `USER` when those variables are present. Add other variable names to the
allowlist when they are required by credentials, toolchains, or remote
execution:

```toml
environment_allowlist = ["GOOGLE_APPLICATION_CREDENTIALS", "JAVA_HOME"]
```

### Redact sensitive text

Configured regular expressions are replaced with `[REDACTED]` before text is
written to summaries, database text fields, or telemetry:

```toml
redaction_patterns = [
  "(?i)authorization: bearer [^\\s]+",
  "(?i)token=[^\\s]+",
]
```

Raw evidence remains local and should still be treated as sensitive.

### Choose a result encoding

```toml
result_encoding = "text"
```

Available values are:

| Value | MCP result |
| --- | --- |
| `text` | Compact JSON in one text content block. This is the default. |
| `toon` | Token-oriented TOON text in one content block. |
| `structured` | MCP structured content only. |
| `both` | Structured content plus backwards-compatible JSON text. |

## Reference

| Setting | Default | Description |
| --- | --- | --- |
| `allowed_roots` | `[]` | Absolute roots containing workspaces the server may access. An empty list allows any workspace. |
| `cache_root` | Platform user cache under `bazel-mcp` | Directory for metadata, logs, and BEP evidence. |
| `bazel_executable` | unset | Explicit Bazel or Bazelisk executable. |
| `output_user_root` | unset | Isolated Bazel output user root managed by the server. |
| `allowed_commands` | build, test, coverage, query commands, and selected informational commands | Commands eligible to run. |
| `denied_commands` | `clean`, `fetch`, `mobile-install`, `run`, `shutdown`, `sync` | Commands rejected even if also present in `allowed_commands`. |
| `environment_allowlist` | `[]` | Additional environment variables passed to Bazel. |
| `redaction_patterns` | `[]` | Regular expressions removed from model-visible and persisted text fields. |
| `global_concurrency` | `4` | Maximum concurrent Bazel invocations. |
| `maximum_pending_invocations` | `256` | Maximum queued and running invocations. Must be at least `global_concurrency`. |
| `default_timeout_seconds` | `1800` | Timeout used when a request omits one. |
| `maximum_timeout_seconds` | `7200` | Maximum timeout accepted from a request. |
| `cancellation_interrupt_grace_seconds` | `10` | Time allowed after the initial interrupt. |
| `cancellation_terminate_grace_seconds` | `5` | Additional time allowed after termination. |
| `progress_initial_seconds` | `30` | Delay before the first MCP progress notification. |
| `progress_interval_seconds` | `60` | Interval between later progress notifications. |
| `retention_days` | `7` | Maximum age of retained invocation evidence. |
| `maximum_storage_bytes` | `10737418240` | Maximum cache size before older evidence is removed. |
| `retention_cleanup_interval_seconds` | `3600` | Interval between retention sweeps. |
| `result_encoding` | `text` | Model-visible result representation. |
| `supported_bazel_major_versions` | `[7, 8, 9]` | Bazel major versions accepted by default. |
| `allow_unsupported_bazel_versions` | `false` | Allow majors outside `supported_bazel_major_versions`. |
| `version_check_timeout_seconds` | `30` | Timeout for the pre-invocation Bazel version check. |
| `isolated_bazel_server_idle_seconds` | `60` | Bazel server idle timeout used with `output_user_root`. |

## CLI options

Run `bazel-mcp --help` for the authoritative command-line reference:

| Option | Description |
| --- | --- |
| `--config <PATH>` | Read configuration from a TOML file. |
| `--allow-root <PATH>` | Add an allowed workspace root. May be repeated. |
| `--cache-root <PATH>` | Override the invocation cache directory. |
| `--log <FILTER>` | Set the tracing filter written to stderr. |

Standard output is reserved for MCP protocol frames. Tracing and diagnostics
are always written to standard error.
