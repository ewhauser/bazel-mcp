# 002: Project Architecture and Repository Setup

## Status

Proposed

## Summary

Set up `bazel-mcp` as a Rust 2024 virtual Cargo workspace with a thin executable,
narrow library crates, one-way dependency flow, a pinned stable toolchain,
centralized dependency metadata, committed lockfiles, and explicit developer,
test, benchmark, fuzz, CI, security, and release surfaces.

The setup follows the conventions used by Shuck:

- `crates/*` workspace membership with shared package metadata and dependencies.
- A checked-in stable `rust-toolchain.toml` containing `rustfmt` and `clippy`.
- A root `Makefile` as the supported developer interface.
- Nix for reproducible auxiliary tools rather than as a replacement for Cargo.
- `cargo fmt`, strict workspace Clippy, `cargo-shear`, `cargo-deny`, and
  `cargo-audit` as separate quality gates.
- Dedicated benchmark and `cargo-fuzz` workspaces.
- Root and targeted nested `AGENTS.md` files, with `CLAUDE.md` symlinked to the
  root instructions.
- Conventional Commits, release-please, cargo-dist, pinned GitHub Action
  digests, and SBOM generation.

This specification makes the conceptual crate list from
`001-product-requirements.md` concrete. It is authoritative for package names,
dependency direction, repository layout, and project tooling. Product behavior
and acceptance requirements remain defined by specification 001.

## Motivation

The product spans several failure and performance domains:

- MCP framing and host compatibility
- asynchronous process execution and cancellation
- Bazel workspace, wrapper, command, and flag policy
- versioned protobuf generation and BEP graph reconstruction
- deterministic build, test, coverage, query, and text reduction
- database-free, crash-recoverable filesystem storage
- benchmarking of both system overhead and model-visible output

Putting all of this in one crate would couple protocol code to subprocess and
persistence details, make reducer tests require an async runtime, and make it easy
for MCP handlers to bypass token and safety boundaries. The workspace needs small
crates with explicit ownership and a single application service that composes
them.

At the same time, the initial repository should not reproduce Shuck-specific
surface area that does not serve this product. There is no Python, WASM, website,
clean-room, or crates.io publishing setup in the initial architecture.

## Design

### Goals

- Make each major subsystem independently testable.
- Keep MCP out of leaf crates and restrict Tokio to async I/O boundaries.
- Keep storage and process side effects out of reducers.
- Prevent circular dependencies through a documented crate DAG.
- Provide one supported command for each common developer operation.
- Make the Bazel-version matrix, BEP fixtures, token benchmark, and MCP
  conformance suite first-class repository assets.
- Produce reproducible macOS, Linux, and Windows x86_64 release binaries with
  an SBOM.
- Preserve the ability to add HTTP transport and remote artifact adapters
  without restructuring the core.

### Non-Goals

- A reusable public SDK in the first release.
- Publishing internal crates to crates.io.
- A Bazel build definition for building `bazel-mcp` itself. Cargo is the source
  of truth for this repository.
- Dynamically loaded native plugins for reducers or storage backends. Custom
  diagnostic reducers use the bounded Starlark adapter described below.
- A second async runtime.
- Full Windows process-tree cancellation in the MVP. A Windows x86_64 preview
  binary uses direct-child termination until job-object support is implemented.
- Copying Shuck's Python, npm, website, oracle, or large shell-corpus workflows.

## Repository layout

The initial repository is laid out as follows:

```text
.
├── .gitattributes
├── .githooks/
│   └── pre-commit
├── .github/
│   ├── ISSUE_TEMPLATE/
│   ├── pull_request_template.md
│   └── workflows/
│       ├── ci.yml
│       ├── fuzz.yml
│       ├── release-please.yml
│       ├── release.yml
│       └── token-integration.yml
├── .gitignore
├── .pre-commit-config-agent.yaml
├── .pre-commit-config.yaml
├── .release-please-config.json
├── .release-please-manifest.json
├── .renovaterc.json5
├── AGENTS.md
├── CHANGELOG.md
├── CLAUDE.md -> AGENTS.md
├── CODE_OF_CONDUCT.md
├── CONTRIBUTING.md
├── Cargo.lock
├── Cargo.toml
├── LICENSE
├── Makefile
├── README.md
├── SECURITY.md
├── deny.toml
├── flake.lock
├── flake.nix
├── rust-toolchain.toml
├── rustfmt.toml
├── crates/
│   ├── bazel-mcp-benchmark/
│   ├── bazel-mcp-bep/
│   ├── bazel-mcp-policy/
│   ├── bazel-mcp-reducer/
│   ├── bazel-mcp-runner/
│   ├── bazel-mcp-server/
│   ├── bazel-mcp-store/
│   ├── bazel-mcp-types/
│   ├── diagnostic-reducer-core/
│   ├── diagnostic-reducer/
│   └── diagnostic-reducer-cli/
├── fuzz/
│   ├── Cargo.lock
│   ├── Cargo.toml
│   └── fuzz_targets/
├── scripts/
│   ├── benchmarks/
│   │   ├── setup-oss-corpus.sh
│   │   └── run-token-integration.sh
│   ├── check-release-please-config.py
│   ├── check-release-security.py
│   ├── fuzz-init.sh
│   ├── generate-release-sbom.sh
│   └── test-bazel-matrix.sh
└── specs/
    ├── 001-product-requirements.md
    └── 002-project-architecture.md
```

`target/`, `.cache/`, generated fuzz corpora, fuzz artifacts, benchmark output,
local MCP configuration, and invocation data are ignored. BEP and diagnostic
fixtures intentionally checked into a crate's `tests/fixtures/` directory are
not ignored.

## Workspace crates

### `bazel-mcp-types`

Leaf crate containing domain types shared across subsystems. It has no filesystem,
process, storage-driver, Tokio, or MCP dependencies.

Responsibilities:

- Invocation UUID, state, timestamps, command metadata, and state transitions.
- Target, diagnostic, test, coverage, artifact, query, and summary types.
- Pagination request/result types and stable sort keys.
- Error categories that cross crate boundaries.
- Serialization of durable and model-independent domain records.

Suggested layout:

```text
crates/bazel-mcp-types/
├── Cargo.toml
└── src/
    ├── artifact.rs
    ├── command.rs
    ├── coverage.rs
    ├── diagnostic.rs
    ├── invocation.rs
    ├── lib.rs
    ├── pagination.rs
    ├── query.rs
    ├── result.rs
    └── test.rs
```

The crate uses `thiserror`, `serde`, and `uuid`. It does not derive MCP schemas;
MCP-specific parameter types belong to `bazel-mcp-server`.

### `bazel-mcp-bep`

Owns Bazel protobuf generation, length-delimited framing, and BEP graph
reconstruction. It is runtime-agnostic and reads from ordinary `Read` values or
byte slices.

Responsibilities:

- Generated Buffa owned messages and borrowed views for the pinned Bazel BEP
  schema.
- Varint frame decoding with explicit incomplete-frame outcomes.
- Event identity, announcement, and reference tracking.
- `NamedSetOfFiles` graph resolution without quadratic expansion.
- Compatibility metadata for the Bazel version used to vendor the protos.
- A self-contained event handle that retains the raw protobuf frame while
  reducers borrow generated views from it without copying protobuf strings,
  bytes, nested messages, or repeated messages.

Suggested layout:

```text
crates/bazel-mcp-bep/
├── AGENTS.md
├── Cargo.toml
├── build.rs
├── proto/
│   ├── LICENSE.bazel
│   ├── README.md
│   └── bazel/...
├── src/
│   ├── event.rs
│   ├── framing.rs
│   ├── generated.rs
│   ├── graph.rs
│   ├── lib.rs
│   └── named_files.rs
└── tests/
    ├── compatibility.rs
    ├── framing.rs
    └── fixtures/
        ├── bazel-8/
        └── bazel-9/
```

`build.rs` invokes a vendored `protoc` binary directly to create a descriptor
set, then uses `buffa-build` to generate owned messages and borrowed views.
Ordinary Cargo builds therefore do not depend on a system protobuf
installation. The script declares `rerun-if-changed` for the vendored proto
tree. Generated Rust is written to `OUT_DIR` and included from `lib.rs`;
generated files are not committed.

The proto `README.md` records the upstream Bazel tag, source paths, update
procedure, and checksums. The Apache-2.0 license accompanying vendored Bazel
files is retained.

### `bazel-mcp-bes`

Owns the loopback gRPC `google.devtools.build.v1.PublishBuildEvent` service.
It uses Buffa owned views for request decoding, validates stream identity and
sequence numbers, and writes Bazel `Any.value` payloads into bounded
varint-delimited BEP evidence. It has no storage or MCP dependency and binds
only to `127.0.0.1` on an ephemeral port.

### `bazel-mcp-policy`

Owns configuration and validation that determines where and how Bazel may run.
It depends only on `bazel-mcp-types` among internal crates.

Responsibilities:

- Load, validate, and merge server configuration.
- Canonicalize and allowlist workspace roots.
- Resolve a configured Bazel wrapper, `tools/bazel`, Bazelisk, or Bazel.
- Classify allowed, opt-in, and denied commands.
- Validate startup arguments, command arguments, and reserved flags.
- Build the minimal child environment.
- Compile and apply secret-redaction rules.
- Calculate the known scheduling key from workspace and output-base policy.

Suggested layout:

```text
crates/bazel-mcp-policy/src/
├── command.rs
├── config.rs
├── environment.rs
├── executable.rs
├── flags.rs
├── lib.rs
├── redaction.rs
└── workspace.rs
```

Policy returns typed decisions. It does not spawn Bazel or write invocation
records.

### `bazel-mcp-store`

Owns durable invocation files, startup-built indexes, cursors, retention, and
crash recovery. It is an async library so filesystem commits compose directly
with the runner without blocking Tokio core workers.

Responsibilities:

- Create private cache and invocation directories.
- Persist requests before process launch.
- Open stdout, stderr, and BEP capture files safely.
- Store versioned invocation metadata and compact structured sidecars only where
  they avoid reparsing genuinely structured results.
- Commit monotonic lifecycle transitions atomically.
- Provide filtered, stable pagination for every inspect view.
- Recover orphaned invocations as `interrupted`.
- Apply age and size retention without deleting live invocations.

Suggested layout:

```text
crates/bazel-mcp-store/
├── Cargo.toml
├── src/
│   ├── cursor.rs
│   ├── files.rs
│   ├── lib.rs
│   └── storage.rs
└── tests/
    ├── pagination.rs
    └── recovery.rs
```

`Store` supports multiple server processes sharing one cache root. A process
lock per invocation serializes its manifest and sidecar commits. Each server
owns one leased epoch marker and publishes a committed mutation by atomically
renaming only that marker, so independent writers do not contend on root
metadata. An owner lease identifies the process responsible for each
nonterminal invocation. One maintenance lock elects recovery and
global-retention work. Each process has a `RwLock`-protected compact index and
refreshes it when the epoch-marker set changes. Index locks are never held
across Bazel execution, so independent
invocations and inspections can proceed concurrently. The index is rebuilt from
versioned `manifest.json` files. UUIDv7 maps deterministically to a day bucket
and bounded shard. Manifest and sidecar commits use write-private-temp plus
atomic rename; deletion uses rename to a cache-root trash directory, immediate
index removal, and unlink outside the index lock. Store methods accept and
return `bazel-mcp-types` and do not mention MCP or BEP protobufs. There is
intentionally no database, migration layer, or legacy-layout reader.

The manifest is the sole durable representation of the redacted request and
compact summary header. Large target, test, and per-file coverage collections
live in `details.json`; artifacts live in `artifacts.json`. Startup does not read
either sidecar. Telemetry counters are accumulated in the index and coalesced at
a bounded interval or into the next durable mutation. A terminal completion
coalesces state, termination, summary, metrics, canonical arguments, artifact
accounting, and detailed results.

Complete stdout, stderr, and BEP evidence remains ordinary files written
directly by the Bazel child. Query pagination scans stdout using opaque byte
offsets, applies bounded redaction before filtering, and never writes a duplicate
normalized query payload.

### `diagnostic-reducer-core`

Provider-neutral, synchronous state machine for incrementally reducing bounded
log streams and trusted structured findings. It is explicitly versioned and
publishable and depends only on general-purpose serialization crates.

Responsibilities:

- Frame arbitrary byte chunks into normalized CR, LF, and CRLF lines.
- Track caller-owned scopes, streams, line spans, parser IDs, and end reasons.
- Execute an immutable ordered plan of trusted native parsers.
- Bound open scopes, retained bytes, line length, and emitted candidates.
- Apply path mapping, redaction, control sanitization, fallback arbitration,
  exact deduplication, typed ranking, and serialized output budgets.
- Return diagnostics, test failures, and text-free accounting.

The crate contains no language grammar or provider semantics and performs no
filesystem, process, storage, network, environment, clock, random, or async
work. Raw evidence retention is a caller responsibility.

### `diagnostic-reducer`

Provider-neutral built-in parser pack and synchronous batch compatibility API.
It depends only on `diagnostic-reducer-core` and serialization crates.

Responsibilities:

- Run the fixed compiler, linter, runtime, and test parser registry in
  deterministic order.
- Parse Rust, C/C++, Java, JavaScript, TypeScript, Python, Go, and protobuf
  diagnostics without build-provider objects.
- Extract bounded Rust, GoogleTest, Go, JavaScript, Java, and Python test
  evidence while refusing incomplete structured blocks.
- Preserve batch/streaming equivalence across arbitrary chunk boundaries.

Runtime plugin discovery and provider-specific log acquisition, path rules,
status messages, and protocol objects are outside its contract.

### `diagnostic-reducer-cli`

Thin I/O and presentation adapter over the generic parser pack. It incrementally
reads stdin or explicitly named files and renders human text, JSON, JSONL,
SARIF, or GitHub workflow annotations. It does not launch commands, discover
provider state, or retain raw evidence. Explicit literal-redaction and
path-prefix rules are passed into the core output policy.

All three generic crates are independently versioned and publishable so they can
move together to a dedicated repository. A repository check enforces their
dependency direction and rejects provider-specific semantics in their Rust
sources.

### `bazel-mcp-reducer`

Pure, deterministic conversion of BEP views and generic reduced diagnostics into
Bazel domain results. It depends on `diagnostic-reducer`, `bazel-mcp-bep`, and
`bazel-mcp-types`, but not on the store, runner, Tokio, or MCP.

Responsibilities:

- Reduce loading, analysis, target, action, build-finished, and metric events.
- Reduce test attempts, shards, summaries, XML, and log excerpts.
- Discover and parse local LCOV data.
- Parse and page query output adapters.
- Map generic diagnostics into Bazel categories, targets, and actions.
- Run explicitly configured Starlark reducers against a bounded, normalized,
  redacted projection after the built-in Rust reducers.
- Apply diagnostic selection and response byte budgets.
- Return explicit fallback reasons when structured evidence is incomplete.

Suggested layout:

```text
crates/bazel-mcp-reducer/
├── AGENTS.md
├── Cargo.toml
├── src/
│   ├── budget.rs
│   ├── build.rs
│   ├── coverage.rs
│   ├── diagnostics/
│   │   ├── generic.rs
│   │   ├── java.rs
│   │   ├── mod.rs
│   │   └── rust.rs
│   ├── lib.rs
│   ├── query.rs
│   ├── test.rs
│   └── text.rs
└── tests/
    ├── snapshots.rs
    └── fixtures/
        ├── coverage/
        ├── logs/
        └── test-results/
```

Reducers receive already bounded evidence inputs. If a reducer needs a file, the
runner reads the permitted file and supplies a bounded reader or buffer. This
keeps filesystem authorization in the runner/policy boundary.

The reducer crate owns a language-neutral extension contract: immutable
invocation context and normalized BEP events in, and a typed headline/diagnostic
patch out. Starlark is an adapter for that contract rather than the core reducer
model. Built-in Rust reducers always run first. An augmenting reducer may add a
headline and diagnostics; an overriding reducer may suppress built-in
diagnostics only for explicitly selected target or action evidence. It cannot
change success, invocation identity, counts, tests, coverage, artifacts,
termination, metrics, or durable raw evidence.

Starlark modules are compiled and frozen at server startup, declare a versioned
API and deterministic selectors, and are ordered by descending priority then
name. The dialect disables `load` and `print` and exposes no filesystem,
environment, network, process, clock, random, or storage functions. Evaluation
has source, input, event, output, item, instruction, heap, call-stack, and
best-effort wall-clock limits. The runner redacts the context before evaluation,
reapplies redaction and the common output budget afterward, and retains the
native result with a bounded note if a custom reducer fails.

Snapshot tests use `insta` with redactions for UUIDs, timestamps, absolute paths,
and platform-dependent details. Snapshot changes require review; tests must not
blindly accept changed diagnostics.

### `bazel-mcp-runner`

The application core. It owns the async runtime-facing invocation lifecycle and
is the only crate that composes policy, storage, BEP, reducers, and child
processes.

Responsibilities:

- Expose `InvocationService` with `run`, `inspect`, and `cancel` operations.
- Queue work by known effective output-base key, acquire a per-user
  cross-process advisory lock for every invocation, and enforce the global
  limit.
- Generate UUIDv7 invocation IDs.
- Select the direct Bazel driver or the optional command-specific Aspect CLI
  driver after policy validation.
- Assemble validated Bazel or Aspect argv without a shell.
- Spawn the selected client in a process group.
- Capture stdout, stderr, and BEP without returning them to the caller.
- Start and register the optional loopback BES transport before spawning Bazel,
  and whenever Aspect routing is enabled so Aspect may forward its owned BEP
  stream to private capture metadata.
- Enforce timeout and graceful cancellation escalation.
- Await async store operations directly; run CPU-heavy BEP/reducer work and
  blocking filesystem work via bounded `spawn_blocking` tasks.
- Observe both advisory and native Bazel output-base waits and include their
  combined duration in concise progress snapshots and invocation metrics.
- Recover stored state during application startup.

Suggested layout:

```text
crates/bazel-mcp-runner/
├── AGENTS.md
├── Cargo.toml
├── src/
│   ├── cancel.rs
│   ├── capture.rs
│   ├── driver.rs
│   ├── inspect.rs
│   ├── lib.rs
│   ├── process.rs
│   ├── progress.rs
│   ├── recovery.rs
│   ├── scheduler.rs
│   └── service.rs
└── tests/
    ├── cancellation.rs
    ├── concurrency.rs
    ├── lifecycle.rs
    └── workspaces/
        ├── analysis-failure/
        ├── build-failure/
        ├── coverage/
        ├── large-output/
        ├── query/
        ├── success/
        └── test-failure/
```

`InvocationService` is the single application boundary presented to MCP. MCP
handlers do not receive raw store or child-process handles.

The runner uses Tokio for processes, signals, time, synchronization, and
blocking-task coordination. `tokio_util::sync::CancellationToken` is the common
cancellation primitive. Unix process-group details are behind a small
platform-specific module with a compile-time Windows stub until runtime support
is implemented.

### `bazel-mcp-server`

Ships the `bazel-mcp` executable and the MCP server library. It is deliberately
thin.

Responsibilities:

- Select stdio MCP or ephemeral agent-mode CLI startup, parse the applicable
  options or Bazel argv, and locate server configuration.
- Initialize tracing without writing application logs to MCP stdout.
- Construct `InvocationService` and run startup recovery.
- Serve MCP over stdio.
- Define and route exactly `bazel.run`, `bazel.inspect`, and `bazel.cancel`.
- Map MCP progress and cancellation to `InvocationService`.
- Encode MCP and ephemeral CLI results from one bounded logical result according
  to the configured result encoding.
- Give agent-mode invocations private temporary store roots and remove them
  before normal CLI exit; only stdio MCP mode runs retention and exposes
  inspection.
- Report tool-execution errors separately from failed Bazel invocations.

Suggested layout:

```text
crates/bazel-mcp-server/
├── Cargo.toml
├── README.md
├── src/
│   ├── args.rs
│   ├── lib.rs
│   ├── logging.rs
│   ├── main.rs
│   ├── server.rs
│   └── tools/
│       ├── cancel.rs
│       ├── inspect.rs
│       ├── mod.rs
│       └── run.rs
└── tests/
    ├── mcp_stdio.rs
    ├── schema_snapshots.rs
    └── tool_contracts.rs
```

The package is named `bazel-mcp-server`, its library target is
`bazel_mcp_server`, and its binary target is `bazel-mcp`.

`main.rs` only parses arguments, initializes logging, calls the library
entrypoint, renders a startup error to stderr, and chooses the process exit code.
The reusable entrypoint is conceptually:

```rust
pub async fn serve(config: ServerConfig) -> anyhow::Result<()>;
```

Only this crate depends on `rmcp` and `schemars`. MCP parameter types map into
domain requests at the handler boundary.

### `bazel-mcp-benchmark`

Non-published benchmark crate modeled after Shuck's dedicated benchmark crate.
It contains Criterion microbenchmarks and shared generated fixtures.

Responsibilities:

- BEP frame and graph throughput.
- Reduction throughput for representative successful, failed, and noisy builds.
- `NamedSetOfFiles` scaling.
- Filesystem record commit, startup rebuild, GC, and inspection throughput.
- Query streaming throughput and peak-memory cases.
- Response byte accounting for golden results.
- Commit-pinned Abseil integration scenarios and three execution adapters.
- Canonical model-visible transcript capture and `tiktoken-rs` accounting.
- JSON and Markdown token-savings reports with enforceable acceptance gates.

Suggested layout:

```text
crates/bazel-mcp-benchmark/
├── Cargo.toml
├── benches/
│   ├── bep.rs
│   ├── named_files.rs
│   ├── query.rs
│   ├── reducer.rs
│   └── store.rs
├── resources/
│   ├── README.md
│   ├── fixtures/...
│   ├── projects/
│   │   └── abseil-cpp.toml
│   └── scenarios/
│       └── abseil-cpp/...
└── src/
    ├── bin/
    │   └── token-integration.rs
    ├── corpus.rs
    ├── lib.rs
    ├── report.rs
    ├── transcript.rs
    └── token_count.rs
```

The crate sets `publish = false`. Large fixtures are generated or produced from
repository-owned test workspaces. Any third-party fixture must include source,
commit, and license metadata. `tiktoken-rs` is a benchmark-crate dependency only;
production request handling does not tokenize responses.

## Dependency direction

The internal dependency graph is acyclic:

```text
bazel-mcp-server
├── bazel-mcp-runner
│   ├── bazel-mcp-policy
│   │   └── bazel-mcp-types
│   ├── bazel-mcp-store
│   │   └── bazel-mcp-types
│   ├── bazel-mcp-reducer
│   │   ├── diagnostic-reducer
│   │   │   └── diagnostic-reducer-core
│   │   ├── bazel-mcp-bep
│   │   └── bazel-mcp-types
│   ├── bazel-mcp-bep
│   ├── bazel-mcp-bes
│   │   └── bazel-mcp-bep
│   └── bazel-mcp-types
└── bazel-mcp-types

bazel-mcp-benchmark
└── may depend on any library crate, but never the reverse

diagnostic-reducer-cli
└── diagnostic-reducer
    └── diagnostic-reducer-core
```

Rules:

- `bazel-mcp-types` has no internal dependencies.
- `diagnostic-reducer-core` has no workspace-internal dependencies and no async
  or I/O surface.
- `diagnostic-reducer` depends only on `diagnostic-reducer-core` among internal
  crates and adds no I/O.
- `diagnostic-reducer-cli` is the only generic crate with filesystem/stdin I/O;
  it depends only on the generic reducer family among internal crates.
- `bazel-mcp-bep` does not depend on application crates.
- `bazel-mcp-bes` depends only on `bazel-mcp-bep` among internal crates.
- `bazel-mcp-policy`, `bazel-mcp-store`, and `bazel-mcp-reducer` are siblings and
  do not depend on each other.
- `bazel-mcp-runner` is the only composition layer below the server.
- `bazel-mcp-server` does not call the store directly, parse BEP, read diagnostic files, or
  spawn Bazel directly.
- No library crate depends on `bazel-mcp-server` or `bazel-mcp-benchmark`.
- A new cross-cutting type moves downward into `bazel-mcp-types`; it is not
  duplicated to avoid a dependency rule.
- Test-only cycles are also avoided. Shared fixture helpers stay local until at
  least three crates need them; only then may a non-published
  `bazel-mcp-test-support` crate be proposed.

## Runtime architecture

```text
MCP stdin/stdout
      │
      ▼
bazel-mcp-server
  - protocol framing
  - tool schemas
  - progress/cancel mapping
      │ InvocationService API
      ▼
bazel-mcp-runner
  - scheduler and live invocation registry
  - per-request cross-process output-base locks
  - Tokio process tasks
  - cancellation tokens
      ├────────────► bazel-mcp-policy
      │               validated workspace, argv, environment, redaction
      ├────────────► Bazel/Bazelisk/repo wrapper
      │               stdout.log + stderr.log + tail events.bep
      ├────────────► bazel-mcp-bes (optional loopback gRPC)
      │               validated ordered BES frame batches
      ├─ spawn_blocking ─► shared BEP capture pipeline
      │                     durable raw frame gate -> reducer subscriber
      └────── await ─────► bazel-mcp-store
                              atomic manifest + detail sidecars
```

The live registry stores only running/queued control state and cancellation
handles. Durable invocation facts live in `bazel-mcp-store`. The registry is not
the source of truth for completed results.

MCP request tasks never hold a global lock while awaiting a child process. The
scheduler owns short critical sections around queue transitions. Each invocation
runs in its own Tokio task. Async record commits are awaited; blocking streaming
reduction and filesystem scans run outside Tokio core workers.

## Cargo workspace configuration

### Root manifest

The repository is a virtual workspace with no root package:

```toml
[workspace]
resolver = "2"
members = ["crates/*"]

[workspace.package]
version = "0.1.0"
edition = "2024"
rust-version = "1.94.1"
license = "MIT"
authors = ["Eric Hauser"]
repository = "https://github.com/ewhauser/bazel-mcp"

[workspace.metadata.cargo-shear]
ignored = ["bazel-mcp-benchmark"]
```

The initial Rust version matches the stable toolchain currently used by Shuck.
The `rust-toolchain.toml`, `workspace.package.rust-version`, and CI environment
are updated together in one change.

All external and internal dependencies are declared under
`[workspace.dependencies]`. Crate manifests use `{ workspace = true }`. Private
internal dependencies use workspace-relative paths. Packages intended for
extraction include both a workspace-relative path and an explicit compatible
version so `cargo package` can resolve their published dependency:

```toml
[workspace.dependencies]
bazel-mcp-bep = { path = "crates/bazel-mcp-bep" }
bazel-mcp-policy = { path = "crates/bazel-mcp-policy" }
bazel-mcp-reducer = { path = "crates/bazel-mcp-reducer" }
bazel-mcp-runner = { path = "crates/bazel-mcp-runner" }
bazel-mcp-store = { path = "crates/bazel-mcp-store" }
bazel-mcp-types = { path = "crates/bazel-mcp-types" }
diagnostic-reducer = { path = "crates/diagnostic-reducer", version = "0.1.0" }
diagnostic-reducer-core = { path = "crates/diagnostic-reducer-core", version = "0.1.0" }
```

External dependencies are grouped by purpose in the root manifest. The initial
set includes:

| Area | Dependencies |
| --- | --- |
| MCP and async | `rmcp`, `tokio`, `tokio-util`, `futures` |
| Serialization and schemas | `serde`, `serde_json`, `schemars`, `uuid` |
| BEP | `buffa`, `buffa-build`, vendored `protoc` support |
| Storage | standard filesystem APIs, `tempfile` for tests |
| Parsing | a bounded XML parser, LCOV parser or internal LCOV reader, `memchr`, ANSI stripping |
| Errors and logging | `thiserror`, `anyhow`, `tracing`, `tracing-subscriber` |
| CLI and platform | `clap`, user-directory resolution, Unix signal/process support |
| Tests | `assert_cmd`, `predicates`, `insta`, `test-case`, `proptest` |
| Benchmarks | `criterion`, `tiktoken-rs` |

Dependency versions are selected and tested during scaffolding, declared once in
the workspace manifest, and committed through `Cargo.lock`. The official Rust
MCP SDK is pinned to a released version rather than a Git branch.

### Package manifests

Every crate inherits common metadata:

```toml
[package]
name = "bazel-mcp-example"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
authors.workspace = true
repository.workspace = true
publish = false
```

All crates are private in the first release. Removing `publish = false` requires
a separate proposal covering public API stability, internal path-plus-version
dependencies, crates.io trusted publishing, and release ordering.

### Profiles

Follow the Shuck profile shape:

```toml
[profile.release]
lto = "thin"
codegen-units = 1
panic = "abort"
strip = "symbols"

[profile.profiling]
inherits = "release"
strip = false
debug = "full"
lto = false

[profile.dist]
inherits = "release"
lto = "thin"
```

Tests and development builds retain unwinding so failures remain diagnosable.

## Toolchain and formatting

`rust-toolchain.toml` pins a stable compiler and required components:

```toml
[toolchain]
channel = "1.94.1"
components = ["rustfmt", "clippy"]
```

The main workspace MUST compile on stable Rust. Nightly is isolated to the
`cargo-fuzz` commands and CI job.

`rustfmt.toml` follows the Shuck defaults:

```toml
edition = "2024"
style_edition = "2024"
newline_style = "Unix"
```

Use default rustfmt layout beyond those settings. Avoid a large collection of
subjective formatting overrides.

`Cargo.lock` is committed because the repository ships an application. The fuzz
workspace has its own committed `fuzz/Cargo.lock`.

## Rust code conventions

- Prefer explicit domain types over JSON values below the MCP boundary.
- `serde_json::Value` is permitted only where the external MCP or Bazel schema is
  genuinely dynamic.
- Library crates define typed errors with `thiserror`. `anyhow` is used at
  executable and orchestration boundaries where context is more valuable than
  enum matching.
- Pure leaf crates remain synchronous. Tokio belongs only in BES, runner,
  server, and store code; the store uses it for async private-file commits.
- Production code does not use `unwrap` or `expect` for recoverable input,
  process, storage, protobuf, or protocol failures.
- Public APIs are narrow and re-exported intentionally from each `lib.rs`.
- Platform code is contained under `cfg` modules rather than scattered through
  the runner.
- New dependencies are added at the root and justified in the PR description.
- Unsafe code is forbidden in workspace-owned source unless a separate design
  documents why a safe dependency cannot provide the required process behavior.
- MCP stdout is protocol-only. Diagnostics and tracing go to stderr or a
  configured file.

## Server configuration

Configuration is user-level and MUST NOT require modifying a target Bazel
repository. It is optional; when no source is present, the server uses built-in
defaults and does not restrict workspace roots.

Resolution order:

1. `--config <path>`
2. `BAZEL_MCP_CONFIG`
3. The OS user config directory, such as
   `~/.config/bazel-mcp/config.toml` or the platform equivalent
4. Built-in secure defaults

Configuration covers:

- allowed workspace roots and per-workspace Bazel wrappers
- allowed and denied commands
- child environment allow/deny rules
- cache directory and retention
- global concurrency and timeouts
- response encoding and byte budgets
- BEP transport (`tail` by default, or explicit loopback `bes`)
- optional Aspect CLI executable, routed commands, and mutation opt-in
- redaction rules
- explicit Starlark reducer files and resource limits
- logging destination and level

The repository contains an `examples/config.toml` once configuration is
implemented. It contains no machine-specific absolute paths or credentials.

The binary starts a stdio MCP server by default. Initial CLI options are limited
to configuration, logging, and version information. Administrative subcommands
such as `doctor` or `gc` may be added later without adding MCP tools.

## Developer interface

The root `Makefile` is the documented interface. It wraps Cargo and scripts; it
does not hide important compiler flags in opaque tooling.

Required targets:

```make
.PHONY: setup-hooks build test test-unit test-integration test-bazel-matrix \
        setup-oss-corpus test-token-integration run check \
        bench bench-save bench-compare bench-token bench-token-live \
        fuzz-setup fuzz-list fuzz-smoke fuzz-run \
        harden-release check-release-security

ARGS ?=
FUZZ_TARGET ?= bep_framing
FUZZ_ARGS ?= -max_total_time=60
NIX_DEVELOP ?= nix --extra-experimental-features 'nix-command flakes' develop --command
OSS_PROJECT ?= abseil-cpp
TOKEN_ENCODING ?= o200k_base
TOKEN_SAMPLES ?= 5

setup-hooks:
	git config core.hooksPath .githooks

build:
	cargo build

test:
	cargo test --workspace --all-features

test-unit:
	cargo test --workspace --all-features --lib

test-integration:
	cargo test --workspace --all-features --tests

test-bazel-matrix:
	$(NIX_DEVELOP) ./scripts/test-bazel-matrix.sh

setup-oss-corpus:
	$(NIX_DEVELOP) ./scripts/benchmarks/setup-oss-corpus.sh $(OSS_PROJECT)

test-token-integration:
	$(NIX_DEVELOP) ./scripts/benchmarks/run-token-integration.sh \
		--project $(OSS_PROJECT) --encoding $(TOKEN_ENCODING) \
		--samples $(TOKEN_SAMPLES) --assert-gates

run:
	cargo run -p bazel-mcp-server -- $(ARGS)

check:
	cargo fmt -- --check
	cargo clippy --workspace --all-targets --all-features -- -D warnings
	$(NIX_DEVELOP) cargo shear

bench:
	cargo bench -p bazel-mcp-benchmark

bench-token: test-token-integration

bench-token-live:
	$(NIX_DEVELOP) ./scripts/benchmarks/run-token-integration.sh \
		--project $(OSS_PROJECT) --encoding $(TOKEN_ENCODING) --live-agent
```

Additional targets wrap Criterion baseline save/compare, fuzz initialization
and smoke tests, release-workflow hardening, and MCP conformance. `bench-token`
is the credential-free deterministic integration benchmark;
`bench-token-live` is its opt-in provider-backed corroboration. Commands printed
in `README.md`, `CONTRIBUTING.md`, and `AGENTS.md` use these targets wherever one
exists.

`make check` remains fast enough for normal iteration. Actual tests are a
separate target so an agent can choose focused tests while working and run the
full suite before handoff.

## Nix development shell

`flake.nix` provides reproducible auxiliary tools for macOS and Linux. It does
not provide the Rust compiler; `rust-toolchain.toml` remains authoritative.

The default shell includes:

- Bazelisk
- Git and a C++17 compiler/toolchain suitable for Abseil
- `cargo-shear`
- `cargo-fuzz`
- `hyperfine`
- filesystem inspection tools such as `find`, `du`, and `jq`
- `jq`
- Python 3 for benchmark and release-security scripts
- Node.js only if required by the pinned MCP conformance suite

`flake.lock` is committed. `nix flake check` validates the development shell in
CI. Bazel versions used by compatibility tests are pinned by the test script and
cached; they are not implicitly taken from an arbitrary developer `PATH`.

## Tests and fixtures

### Unit tests

Unit tests live beside their owning code or under the owning crate's `tests/`
directory. Tests do not need a Bazel installation unless they are explicitly
marked as integration tests.

Ownership:

- BEP framing, graph, and cross-version fixtures belong to `bazel-mcp-bep`.
- Reduction input/output fixtures and snapshots belong to
  `bazel-mcp-reducer`.
- Filesystem commit, retention, recovery, and cursor tests belong to
  `bazel-mcp-store`.
- Policy and argument injection tests belong to `bazel-mcp-policy`.
- Process, cancellation, concurrency, and real workspace tests belong to
  `bazel-mcp-runner`.
- MCP schema and stdio black-box tests belong to `bazel-mcp-server`.

### Fixture rules

- Fixtures are generated from repository-owned miniature Bazel workspaces where
  possible.
- BEP fixtures include the producing Bazel version and exact command in adjacent
  metadata.
- Absolute paths, usernames, hostnames, timestamps, UUIDs, and credentials are
  removed before commit.
- Binary BEP fixtures use a `.bep` extension and are marked binary in
  `.gitattributes`.
- Golden result snapshots use stable ordering and explicit redactions.
- Large generated artifacts stay in `.cache`; only minimal deterministic inputs
  and expected results are committed.

### Bazel compatibility matrix

`scripts/test-bazel-matrix.sh` runs the runner integration workspaces with the
supported Bazel majors from specification 001. It:

1. Resolves a pinned patch version for each supported major.
2. Uses an isolated temporary Bazel user root per version.
3. Runs success, loading failure, analysis failure, action failure, test failure,
   coverage, query, timeout, and cancellation cases.
4. Regenerates no committed fixtures unless an explicit update flag is supplied.
5. Prints a compact version/case table and stores full logs under `.cache`.

Normal unit tests remain fast. The complete matrix runs in CI and before a
release; developers can target one version or case through Make variables.

### Open-source token integration harness

The real-world corpus is Abseil C++, which officially supports Bazel and is
large enough to exercise meaningful loading, analysis, C++ compilation, test,
and query behavior without the infrastructure required by a project such as
Envoy. The initial `resources/projects/abseil-cpp.toml` manifest is conceptually:

```toml
name = "abseil-cpp"
url = "https://github.com/abseil/abseil-cpp.git"
release_tag = "20260526.0"
commit = "5650e9cf76d3be4318d5fa3af38ee483ddfd5e4a"
license = "Apache-2.0"
bazel_version = "9.1.0"
```

The commit, not the tag, is the checkout authority. The tag is human-readable
provenance. `setup-oss-corpus.sh` performs a shallow fetch of that exact commit
into `.cache/corpora/abseil-cpp/<commit>/`, verifies `git rev-parse HEAD`, and
records the checkout metadata. It never runs from an unverified moving branch.
Network access occurs only during this explicit setup step. Normal unit tests
and transcript/tokenizer tests are offline.

Each measured sample gets a clean disposable worktree or copy under
`.cache/benchmarks/work/`. The runner copies a small repository-owned overlay
into that checkout rather than editing Abseil sources. The overlay contains a
separate Bazel package with targets that depend on real Abseil libraries:

- `success`: builds and tests a valid C++ target.
- `compile_failure`: contains one stable, unmistakable C++ type error.
- `test_failure`: builds successfully and fails with a stable assertion message.
- `noisy_failure`: a custom action emits a fixed large set of duplicate warning
  lines followed by one root cause and a nonzero exit.
- `query`: emits a representative dependency or target query result.

The scenario manifest owns exact targets, flags, expected status, and expected
root-cause matchers. A change to the Abseil commit or scenario evidence is a
reviewed fixture update, never an automatic refresh.

The Rust `token-integration` binary implements three adapters behind one trait:

```rust
trait ExecutionAdapter {
    async fn run(&self, scenario: &Scenario, sample: &Sample) -> Result<RunEvidence>;
}
```

- `shell-default` exposes direct terminal output using a recorded host profile:
  10-second initial yield, then 5-second long-process polls.
- `shell-optimized` applies the source discussion's agent instructions and
  Bazel output configuration: 30-second initial yield, a 30-second first poll,
  60-second subsequent polls, `--color=no`, `--curses=no`, a 60-second progress
  rate limit, and test output only on errors.
- `bazel-mcp` invokes the built stdio server through the MCP client boundary,
  waits for completion, and uses at most one narrow `bazel.inspect` call when
  the default result does not contain the expected cause.

Adapter-specific presentation flags are allowed, but semantic Bazel inputs are
identical. Each adapter uses the manifest's Bazel version, target, environment,
task text, and cache condition. It receives an isolated `--output_user_root` to
prevent one adapter from warming another. The harness runs cold and warm suites
separately, randomizes adapter order from a recorded seed, performs one warm-up,
then collects at least five measured samples. It reports medians and p95s and
keeps raw observations so results can be re-aggregated.

Every adapter writes the same JSONL transcript schema. Events contain a sequence
number, adapter, scenario, event kind, role, visibility flag, and content. Rust
struct serialization fixes field order; UTF-8, LF line endings, stable JSON
escaping, normalized workspace paths, and normalized volatile IDs make the
model-visible representation reproducible. Raw logs, BEP, timing, and hashes are
separate evidence fields and are not counted unless an adapter actually exposed
their contents to the simulated model.

`token_count.rs` uses the `tiktoken-rs` singleton for the selected encoding. The
default is `o200k_base`, which covers current GPT and Codex model families. It
computes:

```text
visible_tool_tokens = sum(tokens(tool result content exposed to the model))

cumulative_context_tokens = sum(
  tokens(common prompt + adapter instructions + tool schemas + prior transcript)
  immediately before each simulated model event
)

reduction_percent = 100 * (1 - bazel_mcp / shell_default)
```

The common task prompt is byte-identical across adapters. Adapter-specific tool
schemas and the optimized instruction block are included, so neither MCP nor
prompt overhead is hidden. The report includes the tokenizer crate version,
encoding, canonicalization version, full corpus commit, Bazel version, platform,
compiler, cache condition, sample seed, absolute counts, and ratios.

`--assert-gates` fails unless the aggregate suite meets specification 001's
token, byte, diagnostic-fidelity, and wall-time requirements. It also fails on
an absent scenario, wrong expected exit status, missing root cause, malformed
transcript, unknown tokenizer encoding, or corpus mismatch. This is a
deterministic OpenAI-tokenizer estimate, not provider billing. `--live-agent`
uses the same scenario/task manifests and records provider-reported tokens when
credentials are explicitly supplied.

### MCP conformance

The official MCP conformance suite is pinned and invoked by a script or Make
target. It runs against a built `bazel-mcp` stdio server using a temporary config
and workspace. Upgrading `rmcp` includes a conformance run and tool-schema
snapshot review.

## Fuzzing

Fuzzing follows Shuck's separate root `fuzz/` workspace pattern so nightly and
sanitizer requirements do not affect the stable main workspace.

Initial targets:

- `bep_framing`: arbitrary and truncated varint-delimited inputs
- `bep_event_stream`: arbitrary framed protobuf streams
- `named_file_sets`: nested, repeated, and adversarial file-set graphs
- `diagnostic_reducer`: arbitrary terminal bytes and ANSI sequences through
  both batch and streaming sessions, including arbitrary chunk boundaries,
  scope end reasons, and input/line/candidate bounds
- `redaction`: arbitrary text and configured secret patterns
- `cursor_decode`: arbitrary inspection cursors
- `lifecycle_sequence`: generated state and cancellation sequences

`make fuzz-smoke` initializes deterministic corpora and runs every blocking target
with a fixed, short execution count. Mutation-heavy fuzzing runs on a scheduled
GitHub workflow and uploads minimized artifacts on failure.

Fuzz corpora and artifacts are generated and ignored. Durable seeds are small,
reviewed, and sourced from repository fixtures.

## Benchmarks

There are three benchmark levels.

### Microbenchmarks

Criterion benchmarks in `bazel-mcp-benchmark` measure framing, graph building,
reduction, storage, and query processing. They report bytes or events per second
and include scaling cases, not only fixed examples.

The generic parser pack additionally benchmarks batch input and 64 KiB/1 KiB
streaming chunks over large no-match and mixed-tail logs. It is an explicit
target (`make bench-generic-reducers`) rather than an ordinary unit test.

### Process macrobenchmarks

Scripts under `scripts/benchmarks/` use `hyperfine` or a purpose-built driver to
compare direct Bazel execution with `bazel-mcp` over the same generated
workspaces. They measure Bazel wall time, server overhead, raw output bytes, BEP
bytes, peak RSS, and model-visible result bytes.

### Agent token benchmark

`make bench-token` implements the three-way comparison required by specification
001:

1. Normal terminal tool baseline
2. Optimized terminal orchestration baseline
3. `bazel-mcp`

This command is the credential-free Abseil integration run described in the
test-harness section. The Rust driver executes all adapters, writes canonical
transcripts under `.cache/benchmarks/<run-id>/transcripts/`, and generates
`report.json` plus `report.md`. With `--assert-gates`, it verifies token savings,
model-visible byte savings, diagnostic fidelity, and Bazel wall-time overhead.

`make bench-token-live` runs the same tasks through a configured agent platform
and appends actual input, cached, uncached, and output token metrics where the
platform exposes them. It does not commit prompts, source, logs, credentials, or
provider responses from proprietary repositories. Paid runs require explicit
credentials and are never a blocking pull-request check.

Criterion baseline save/compare commands are exposed through `make bench-save`
and `make bench-compare` and write to a configurable scratch target directory.

## Pre-commit hooks and agent instructions

`make setup-hooks` configures `core.hooksPath=.githooks`. The hook uses `prek`
when available and falls back to `PATH`, matching the Shuck setup. Human and
non-interactive sessions may select separate pre-commit configuration files, but
both run at least formatting and Clippy. Long Bazel matrix and token benchmarks
do not run in pre-commit.

The root `AGENTS.md` contains:

- Project purpose and product invariants
- Supported build, test, lint, benchmark, and fuzz commands
- Crate dependency direction
- MCP stdout/logging rule
- Bazel command execution guidance for working on this repository
- Fixture redaction and update procedures
- Conventional Commit and release policy

`CLAUDE.md` is a symlink to `AGENTS.md` so instructions have one source of truth.

Nested instructions are added only where local guardrails materially differ:

- `bazel-mcp-bep/AGENTS.md`: protobuf provenance, generated code, compatibility,
  and adversarial parser rules.
- `bazel-mcp-reducer/AGENTS.md`: deterministic ordering, byte budgets,
  diagnostic fidelity, and snapshot review rules.
- `diagnostic-reducer-core/AGENTS.md`: provider neutrality, synchronous parser
  lifecycle, hard bounds, transformation order, and publishable API rules.
- `diagnostic-reducer/AGENTS.md`: fixed parser order, generic grammar, redaction,
  batch/streaming equivalence, and reviewed fixture rules.
- `diagnostic-reducer-cli/AGENTS.md`: thin I/O/presentation boundary and no
  process execution.
- `bazel-mcp-runner/AGENTS.md`: process groups, cancellation, blocking-task,
  locking, and no-shell rules.

The root instructions apply first. Nested files do not repeat the entire root
document.

## Continuous integration

GitHub Actions follow these repository-wide rules:

- Actions are pinned by full commit digest with a version comment.
- Checkout uses `persist-credentials: false` except an explicitly justified
  release write job.
- Each workflow and job has minimal permissions and a timeout.
- Pull-request CI uses concurrency groups with `cancel-in-progress: true`.
- Release workflows do not cancel in progress.
- Cargo incremental compilation is disabled in CI; dependency caching is enabled.

### `ci.yml`

Required jobs:

| Job | Purpose |
| --- | --- |
| `nix-flake-check` | Validate the locked development environment. |
| `zizmor` | Audit GitHub Actions and release hardening. |
| `lint` | Run `make check`. |
| `cargo-deny` | Check licenses, bans, and dependency sources. |
| `cargo-audit` | Check the committed lockfile for advisories. |
| `cargo-test-linux` | Full workspace tests and doc tests on Linux. |
| `cargo-test-macos` | Full workspace tests on macOS. |
| `cargo-check-windows` | Compile all targets/features; runtime tests remain deferred. |
| `bazel-matrix` | Run real-workspace integration cases across Bazel majors. |
| `mcp-conformance` | Run the pinned MCP conformance suite and schema snapshots. |
| `fuzz-smoke` | Run deterministic smoke iterations for blocking fuzz targets. |

Performance comparison may post a non-blocking PR report once stable. Paid agent
token benchmarks are not part of normal pull-request CI.

### `token-integration.yml`

Runs on a weekly schedule and manual dispatch. It restores or creates the
commit-pinned Abseil cache, verifies the checkout, runs
`make test-token-integration`, and uploads the JSON report, Markdown report,
canonical transcripts, and bounded failure evidence. It has no provider
credentials. The job is non-blocking for pull requests because the cold C++
build is comparatively expensive; transcript canonicalization, tokenizer
snapshots, scenario-manifest validation, and report arithmetic remain covered by
ordinary workspace tests. A release candidate MUST have a passing integration
report for the exact code being released.

### `fuzz.yml`

Runs on a schedule and manual dispatch with one target per matrix entry. It uses
nightly only inside the fuzz workspace and uploads artifacts on failure. A final
job may create or update an issue when scheduled fuzzing fails.

### Supply-chain checks

`deny.toml` starts from the same posture as Shuck:

- Allow a reviewed set of permissive licenses.
- Deny unknown registries and unknown Git sources.
- Warn on duplicate dependency versions.
- Keep advisory ignores empty unless an entry documents owner, reason, and
  removal condition.

Renovate is security-oriented, pins GitHub Action digests, groups action updates,
and waits three days before adopting newly released Cargo dependencies unless a
security fix requires faster action.

## Release architecture

The project uses Conventional Commits. Pull requests are squash-merged, so the
PR title becomes the release-driving commit on `main`.

Common scopes are crate or area names:

```text
feat(server): add MCP task execution
fix(runner): reap cancelled Bazel clients
perf(reducer): avoid duplicate diagnostic buffers
docs(specs): define remote artifact adapter boundary
chore(deps): update buffa
```

release-please maintains a draft release PR, updates
`workspace.package.version` and `CHANGELOG.md`, and creates a `vX.Y.Z` tag after
merge. Versions and the changelog are not edited manually.

cargo-dist builds the `bazel-mcp` binary for:

- `aarch64-apple-darwin`
- `x86_64-apple-darwin`
- `x86_64-pc-windows-msvc` (preview)
- `aarch64-unknown-linux-gnu`
- `aarch64-unknown-linux-musl`
- `x86_64-unknown-linux-gnu`
- `x86_64-unknown-linux-musl`

Installers are shell, PowerShell, and Homebrew. Windows process-tree
cancellation remains a later compatibility milestone in specification 001.

The release includes:

- Platform archives and checksums
- Shell installer
- Homebrew formula update
- CycloneDX SBOM generated from the locked workspace
- Release notes produced from Conventional Commit history

The cargo-dist workflow is generated, then checked for pinned action digests and
least-privilege permissions by `scripts/check-release-security.py`. The generated
workflow is committed.

There is no crates.io publish workflow initially because all crates are private.

## Documentation and governance files

- `README.md`: outcome-first overview, install instructions, MCP host examples,
  minimal configuration, three-tool summary, and development quick start.
- `CONTRIBUTING.md`: prerequisites, `make` workflow, tests, fixtures, fuzzing,
  Conventional Commits, and release flow.
- `SECURITY.md`: supported versions, vulnerability reporting, local execution
  threat model, and secret/log handling.
- `CODE_OF_CONDUCT.md`: contributor expectations.
- `CHANGELOG.md`: release-please owned.
- `LICENSE`: MIT for workspace-owned source.
- `.github/pull_request_template.md`: summary, changes, test plan, risk notes,
  and checkboxes for `make check`, focused/full tests, Bazel matrix, fixture
  updates, and manual MCP exercise where applicable.

## Setup sequence

Scaffolding proceeds in dependency order:

1. Add governance, toolchain, workspace manifest, lockfile, formatting, deny,
   ignore, Nix, hooks, and Makefile files.
2. Create `bazel-mcp-types` and freeze lifecycle/domain terminology.
3. Create `bazel-mcp-bep`, vendor the pinned proto set, and land cross-version
   framing fixtures.
4. Create policy, async filesystem store, and reducer sibling crates with no
   cross-dependencies.
5. Create the runner and wire process capture to durable storage and reduction.
6. Create the thin server with the three tool schema snapshots and stdio
   black-box test.
7. Add real Bazel workspaces, the version-matrix script, and MCP conformance.
8. Add benchmark and fuzz workspaces, pin Abseil, and land the offline
   `tiktoken-rs` integration harness.
9. Add CI, release-please, cargo-dist, security checks, and SBOM generation.
10. Run `make check`, `make test`, `make test-bazel-matrix`, MCP conformance, and
    `make test-token-integration` before declaring the scaffold complete.

Each step lands a compiling workspace. Placeholder crates expose only the types
needed by the next layer rather than speculative APIs.

## Architecture acceptance criteria

- `cargo metadata` reports exactly the eight initial workspace packages and no
  dependency cycles.
- Only `bazel-mcp-server` depends on `rmcp`.
- Among production library and binary dependencies, only `bazel-mcp-runner`,
  `bazel-mcp-server`, and `bazel-mcp-store` depend directly on Tokio. Benchmark
  and test-only targets may use Tokio to drive the runner API.
- Production storage is database-free and contains no SQL driver dependency.
  Store APIs are async and atomic-commit/crash-recovery tests pass on supported
  filesystems.
- `bazel-mcp-reducer` and `bazel-mcp-store` do not depend on each other.
- All packages inherit version, edition, Rust version, license, authors, and
  repository metadata from the workspace.
- Main workspace and fuzz lockfiles are committed.
- `make check` runs format checking, strict all-target/all-feature Clippy, and
  unused-dependency checking.
- `make test` runs all workspace features successfully on Linux and macOS.
- Windows all-feature compilation succeeds even though runtime support is
  deferred.
- Real Bazel integration tests do not run as ordinary unit tests and are
  available through one documented Make target.
- The Abseil manifest pins tag `20260526.0`, full commit
  `5650e9cf76d3be4318d5fa3af38ee483ddfd5e4a`, Bazel `9.1.0`, and Apache-2.0
  provenance; setup refuses a mismatched checkout.
- `make test-token-integration` runs without model credentials, emits canonical
  JSONL plus JSON/Markdown reports, records `tiktoken-rs` and encoding versions,
  and enforces specification 001's release gates.
- MCP stdout contains no tracing or application text.
- BEP generated Rust is not committed; proto source, provenance, and license are.
- All raw/golden fixtures are redacted and owned by a specific crate.
- Fuzz and benchmark outputs remain ignored.
- GitHub Actions use pinned digests, least privilege, timeouts, and safe checkout
  credential settings.
- release-please owns versions/changelog, cargo-dist owns binary artifacts, and
  release output includes an SBOM.
- `AGENTS.md` is the instruction source of truth and `CLAUDE.md` resolves to it.

## Deferred decisions

- Whether an HTTP transport should be a feature of `bazel-mcp-server` or a
  separate binary package after stdio is stable.
- Whether a remote artifact abstraction belongs in a new
  `bazel-mcp-artifacts` crate once the first CAS/BuildBuddy adapter is designed.
- Whether any internal crate has a public API worth publishing independently.
- Whether Windows runtime support belongs inside `bazel-mcp-runner` or a small
  platform process crate.
- Which agent platform should be the first live provider-metrics corroboration
  target; this does not affect the offline release gate.

## References

- [Product requirements](001-product-requirements.md)
- [Cargo workspaces](https://doc.rust-lang.org/book/ch14-03-cargo-workspaces.html)
- [Official Rust MCP SDK](https://github.com/modelcontextprotocol/rust-sdk)
- [Bazel Build Event Protocol](https://bazel.build/remote/bep)
- [`tiktoken-rs`](https://github.com/zurawiki/tiktoken-rs)
- [Abseil C++](https://github.com/abseil/abseil-cpp)
- [cargo-dist](https://opensource.axo.dev/cargo-dist/)
- [release-please](https://github.com/googleapis/release-please)
