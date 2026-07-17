# Reducer integration testing

Reducer integration cases connect a real, intentionally failing Bazel target to
the exact reducer output checked by ordinary tests. The framework keeps the
human-facing examples separate from the deterministic evidence corpus:

```text
examples/<workspace> -> bazel.run over MCP -> sanitized recording
                                             -> semantic contract
                                             -> exact replay golden
```

All live execution goes through `bazel-mcp`. Replay tests do not start Bazel or
download a language toolchain.

## Repository layout

- `examples/` contains isolated, pinned Bzlmod workspaces. Each has a successful
  target and explicitly selected failing targets tagged `manual` and
  `reducer-fixture`.
- `testdata/reducer-corpus/<language>/<family>/<case>/case.toml` is the versioned
  contract for one failure. Its sibling files are sanitized raw evidence,
  provenance, and the canonical reducer result.
- `testdata/reducer-case.schema.json` is the generated JSON Schema for manifests.
- `crates/bazel-mcp-reducer-cases` implements discovery, validation, MCP live
  runs, recording, replay, semantic assertions, and golden acceptance.

Nested example workspaces deliberately do not share the root module graph. This
keeps a Node toolchain from affecting a Go case and lets live CI select only the
dependency islands touched by a change.

## Common commands

Build the server before a live command, then use the harness:

```sh
make reducer-cases-list
make test-reducer-corpus

cargo build -p bazel-mcp-server --bin bazel-mcp
cargo run -p bazel-mcp-reducer-cases -- verify --live --tag live-smoke
cargo run -p bazel-mcp-reducer-cases -- verify --live --case go/compiler/type-mismatch
```

Selection can combine repeated `--case`, `--tag`, or `--workspace` arguments.
CI uses `--changed-from <git-ref>`: an example or case-directory change selects
the affected live cases, while changes to shared reducer, runner, server, schema,
or harness code select the representative `live-smoke` cases. Replay CI always
checks the full corpus. `--allow-empty` is intended only for CI shards where no
selected case is valid.

`--bazel-version` overrides a case's recorded version for compatibility testing.
`--server`, `--bazel`, and `--runtime-parent` make CI paths explicit. Unsupported
platforms are reported as skipped rather than silently treated as passing.

## Adding a case

1. Add or reuse an isolated workspace under `examples/`. Pin Bazel, language,
   rule, and tool versions. Keep `//:success` buildable and put the intentional
   failure in a small target under `cases/`.
2. Add a `case.toml` under `testdata/reducer-corpus`. Case IDs and directories
   should use `<ecosystem>/<failure-family>/<scenario>`.
3. Write the semantic contract before recording. Assert the actionable primary
   diagnostic, structured location, category, target or action, artifact and
   inspection behavior, suppression requirements, and conservative byte/item
   budgets.
4. Add provenance for every pinned tool and rule. An OSS-derived fixture must
   set both the immutable origin repository and commit.
5. Record, inspect every `actual.*` file, accept explicitly, and replay the
   complete corpus.

A minimal manifest has this shape:

```toml
schema_version = 1
id = "cpp/compiler/type-mismatch"
workspace = "examples/cpp"
command = "build"
args = ["//cases:type_mismatch"]
tags = ["cpp", "compiler", "fast"]
platforms = ["linux", "macos"]
timeout_seconds = 180

[expect]
state = "failed"
exit_code = 1
max_visible_bytes = 4096
max_diagnostics = 10

[[expect.diagnostics]]
rank = 0
severity = "error"
category = "compilation"
message_contains = "cannot initialize"
path = "cases/type_mismatch.cc"
line = 2

[expect.absent]
message_contains = ["Build did NOT complete successfully"]
raw_contains = ["SECRET_SENTINEL"]

[provenance]
tool = "clang"
tool_version = "recorded by the platform toolchain"
```

Run `make generate-reducer-case-schema` only after intentionally changing the
Rust manifest types. `make check-reducer-case-schema` fails if the checked-in
schema is stale. Manifests reject unknown fields and unsupported schema versions.

## Recording and accepting evidence

Recording never overwrites a canonical golden:

```sh
make record-reducer-case REDUCER_CASE=cpp/compiler/type-mismatch
git diff -- testdata/reducer-corpus/cpp/compiler/type-mismatch

BAZEL_MCP_ACCEPT_REDUCER_CASES=1 \
  make accept-reducer-case REDUCER_CASE=cpp/compiler/type-mismatch
```

The first command starts the current server over stdio, initializes MCP, calls
`bazel.run`, and obtains retained views with `bazel.inspect`. It uses isolated
cache, output, Bazelisk, and runtime roots. It writes `actual.*` files only after
the live result satisfies the semantic contract and the sanitized replay agrees
with it.

Acceptance requires both the environment gate and the CLI's `--yes` gate. It
validates evidence, provenance, semantics, exact replay serialization, and
batch-versus-streaming equivalence before replacing canonical files. Review the
diff as product behavior: ordering, root-cause rank, structured fields, bounded
detail, suppressed wrappers, artifacts, and inspection hints all matter.

When reducer code changes but the retained evidence does not, avoid an
unnecessary live run. `make record-reducer-replay REDUCER_CASE=<id>` writes only
`actual.expected.json` from the existing evidence. Review it and use the same
gated accept command. Canonical replay removes volatile invocation and test-case
durations while preserving every behavioral field.

Do not edit a binary BEP recording by hand. Re-record it through MCP. Text and
JSON files may be inspected normally, but canonical changes should still come
through the accept workflow so all gates run.

## Sanitization and provenance

The recorder replaces workspace, home, runtime, cache, and Bazel output roots
with stable markers in text and length-preserving markers in binary protobuf
payloads. Volatile Rust test process IDs are canonicalized. The verifier rejects
absolute home paths, common credential formats, signed URLs, authorization
headers, private keys, and explicit secret sentinels in every checked-in evidence
file and canonical result.

Every recording has `provenance.json` with its case, workspace, command and
arguments, effective Bazel version, platform, architecture, tool versions, rule
versions, and optional immutable OSS origin. A provenance change is reviewed
alongside the evidence it explains.

If a new tool emits another nondeterministic or sensitive form, extend and test
the sanitizer before accepting the fixture. Never weaken secret detection merely
to bless a recording.

## What replay verifies

Each checked-in case verifies:

- exact canonical summary and artifacts;
- state, exit status, headline, and inspection hint;
- diagnostic rank, severity, category, message, location, target, action, and
  repetition count;
- declared artifacts and negative expectations;
- serialized item and model-visible byte budgets;
- secret and absolute-path absence;
- equality between batch and streaming BEP reduction.

Live verification repeats the semantic contract and compares the live summary
with sanitized replay. The comparison permits only documented representation
differences, such as canonical path markers and promotion of the same diagnostic
to test-scoped evidence after test-log inspection.

## CI tiers

- `reducer-replay` runs every recorded case on Linux, macOS, and Windows without
  Bazel and checks schema freshness.
- `reducer-live-changed` runs affected example workspaces through MCP on pull
  requests.
- `reducer-live-smoke` runs a small representative case after pushes to `main`.
- `reducer-live-platform` runs fast cases on Linux, macOS, and Windows with the
  supported Bazel 8 and 9 lines on the schedule or manual dispatch.
- `reducer-live-heavy` is explicit and accepts only cases tagged `heavy`. Browser,
  registry, database, and provider-dependent cases belong here.

Keep PR cases hermetic and credential-free. Mark a platform unsupported in the
manifest when the example genuinely cannot execute there; do not use a broad CI
skip to hide a semantic regression.

## Maintenance checklist

A reducer failure family is covered when its pinned example reproduces the real
failure, live MCP output satisfies the semantic contract, sanitized evidence and
provenance are checked in, the exact golden is reviewed, streaming equals batch,
output remains bounded, generic wrappers do not displace the root cause, secrets
are absent, and supported platforms are explicit.

When a replay changes unexpectedly, inspect the exact golden diff first. When
only live verification changes, compare provenance and toolchain output before
changing assertions. Tighten semantic contracts when a regression escaped; do
not turn stable root-cause requirements into loose substring checks to absorb
unexplained drift.
