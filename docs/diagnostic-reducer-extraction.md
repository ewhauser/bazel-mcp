# Diagnostic reducer repository extraction plan

The generic diagnostic reducer is intentionally implemented as three
independently versioned crates inside this repository before being moved to a
dedicated GitHub repository. The destination repository and organization should
be selected during the extraction PR; no code depends on a guessed URL.

## Packages to move

Move these directories together without changing their public contracts:

- `crates/diagnostic-reducer-core`
- `crates/diagnostic-reducer`
- `crates/diagnostic-reducer-cli`

Also move or recreate:

- the generic streaming/batch equivalence tests and Criterion benchmark;
- the `diagnostic_reducer` fuzz target and durable generic seeds;
- `docs/diagnostic-reducer.md` as package-level architecture documentation;
- the dependency and semantic boundary check;
- formatting, Clippy, test, fuzz-smoke, packaging, and benchmark CI.

Do not move `bazel-mcp-reducer`, BEP types, reducer-case infrastructure, raw
invocation storage, process execution, MCP code, or Bazel fixtures. They remain
downstream consumers in this repository.

## Stable boundary

The extracted dependency graph is:

```text
diagnostic-reducer-cli -> diagnostic-reducer -> diagnostic-reducer-core
```

`diagnostic-reducer-core` may depend only on general-purpose serialization.
`diagnostic-reducer` may additionally depend on the core. The CLI may depend on
both through the parser-pack facade and on general-purpose argument parsing and
serialization. None may contain Bazel path markers, status sentences, action
mnemonics, protocol objects, storage access, process execution, async work, or
filesystem/environment discovery.

The public compatibility surface consists of the serialized finding model,
parser lifecycle, scope/end-reason semantics, transformation order, exact
deduplication rules, ranking keys, budget accounting, built-in parser order, and
CLI output schemas. Version these contracts with SemVer. Adding fields to
serialized types or rules to the parser plan requires fixtures that demonstrate
old and new behavior; breaking model or lifecycle changes require a major
version.

## Extraction sequence

1. Create the destination repository with the same Rust toolchain, license, and
   security policy.
2. Copy the three crates and generic tests, fuzz target, benchmark, docs, and
   boundary check while preserving history where practical.
3. Replace workspace dependencies with ordinary versioned dependencies and run
   `cargo package` for all three packages.
4. Publish in dependency order: core, parser pack, then CLI.
5. In this repository, replace the path dependencies with exact compatible
   crates.io or Git revisions and remove the copied generic sources.
6. Run the generic suite in the new repository and the Bazel 8/9 goldens plus
   reducer corpus here. No expected Bazel golden should change solely because
   of the move.
7. Add automated dependency update PRs and a cross-repository compatibility
   fixture if the parser pack will evolve independently of the Bazel adapter.

Until the move, every generic crate has an explicit `0.1.0` package version and
publishable metadata. `make check-diagnostic-reducer-boundary` prevents
accidental coupling, and `cargo package --list` verifies that each package can
be assembled independently.
