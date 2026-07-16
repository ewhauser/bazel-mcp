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
written to summaries, durable metadata, or telemetry:

```toml
redaction_patterns = [
  "(?i)authorization: bearer [^\\s]+",
  "(?i)token=[^\\s]+",
]
```

Raw evidence remains local and should still be treated as sensitive.

### Choose BEP transport

```toml
bep_transport = "tail"
```

`tail` is the backwards-compatible default. Bazel writes the private binary
BEP file directly, so an existing remote `--bes_backend` or BuildBuddy setup
continues to receive events.

`bes` starts a plaintext gRPC Build Event Service on an ephemeral loopback
port and configures Bazel to publish to it with
`--bes_upload_mode=wait_for_upload_complete`. The service validates the
invocation ID and stream sequence with Buffa views, then reconstructs the same
private varint-delimited `events.bep` file used by reducers and inspection.
Select this mode explicitly because Bazel supports only one `--bes_backend`;
caller-supplied remote BES flags are rejected in this mode. The listener is
never exposed outside the local host.

See [BEP transport performance](bep-transport-performance.md) for benchmark
methodology, current results, and reproduction commands.

### Choose a result encoding

```toml
result_encoding = "toon"
```

Available values are:

| Value | MCP result |
| --- | --- |
| `text` | Compact JSON in one text content block. |
| `toon` | Token-oriented TOON text in one content block. This is the default. |
| `structured` | MCP structured content only. |
| `both` | Structured content plus backwards-compatible JSON text. |

### Configure negotiated task execution

```toml
mcp_execution_policy = "auto"
task_ttl_seconds = 86400
task_poll_interval_ms = 2000
```

`auto` is the recommended default. The server discovers support at runtime and
uses synchronous execution for ordinary clients, the experimental task flow
for clients negotiating MCP `2025-11-25` and sending `params.task`, or the
`io.modelcontextprotocol/tasks` extension for clients negotiating its
`2026-06-30` base protocol and declaring the extension in per-request
capabilities. The task dialect is never selected from a host name.

`sync_only` always returns an ordinary `CallToolResult` for new calls. Existing
unexpired task handles remain readable and cancellable after a restart or
policy change. `tasks_required` rejects `bazel.run` before starting Bazel when
the client did not declare a compatible task flow.

The TTL is the minimum time a terminal task result remains available. A task
that is still queued or running never expires. The poll interval is advisory
and must be between 100 and 60,000 milliseconds.

### Load custom reducers

Built-in Rust reducers remain active when custom reducers are configured.
Starlark files must be listed explicitly:

```toml
[starlark]
files = ["reducers/custom_compiler.star"]
```

Relative paths are resolved against the directory containing the configuration
file and canonicalized at startup. Missing, duplicate, invalid, or incompatible
reducers prevent startup. Bazel workspaces are never searched for reducer files,
so merely checking out a repository cannot execute its code in the server.

All Starlark limits are operator-configurable under `[starlark]`, although the
defaults are intended for ordinary diagnostic reducers. Runtime failures keep
the native result and add a bounded note. See the
[custom reducer guide](custom-reducers.md) for the API and security model.

## Reference

| Setting | Default | Description |
| --- | --- | --- |
| `allowed_roots` | `[]` | Absolute roots containing workspaces the server may access. An empty list allows any workspace. |
| `cache_root` | Platform user cache under `bazel-mcp` | Directory for metadata, logs, and BEP evidence. |
| `bep_transport` | `tail` | BEP ingestion path: private binary file (`tail`) or loopback Build Event Service (`bes`). |
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
| `mcp_execution_policy` | `auto` | New-run policy: `auto`, `sync_only`, or `tasks_required`. |
| `task_ttl_seconds` | `86400` | Minimum terminal task-result availability window; must be greater than zero. |
| `task_poll_interval_ms` | `2000` | Suggested task polling interval, from 100 through 60,000 ms. |
| `retention_days` | `7` | Maximum age of retained invocation evidence. |
| `maximum_storage_bytes` | `10737418240` | Maximum cache size before older evidence is removed. |
| `retention_cleanup_interval_seconds` | `3600` | Interval between retention sweeps. |
| `result_encoding` | `toon` | Model-visible result representation. |
| `supported_bazel_major_versions` | `[8, 9]` | Bazel major versions accepted by default. |
| `allow_unsupported_bazel_versions` | `false` | Allow majors outside `supported_bazel_major_versions`. |
| `version_check_timeout_seconds` | `30` | Timeout for the pre-invocation Bazel version check. |
| `isolated_bazel_server_idle_seconds` | `60` | Bazel server idle timeout used with `output_user_root`. |
| `starlark.files` | `[]` | Explicit custom reducer files. Relative paths are resolved from the configuration file. |
| `starlark.max_source_bytes` | `262144` | Maximum UTF-8 source size per reducer. |
| `starlark.max_input_bytes` | `1048576` | Maximum normalized stdout/stderr input and the basis for bounded baseline data. |
| `starlark.max_events` | `10000` | Maximum normalized BEP events retained for custom reducers. |
| `starlark.max_output_bytes` | `65536` | Maximum serialized patch size per reducer. |
| `starlark.max_output_items` | `1000` | Maximum diagnostics returned per reducer. |
| `starlark.max_ticks` | `1000000` | Starlark evaluator instruction budget. |
| `starlark.max_heap_bytes` | `16777216` | Starlark evaluator heap budget. |
| `starlark.max_callstack_size` | `100` | Maximum Starlark call-stack depth. |
| `starlark.timeout_ms` | `100` | Best-effort wall-clock evaluation limit per reducer. |

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
