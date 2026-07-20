# Contributing

Install the pinned Rust toolchain, Bazelisk, Git, a C++ toolchain, and the tools
provided by `nix develop`. Run `make setup-hooks` once per checkout.

Use the root Makefile as the stable interface. Before opening a pull request,
run `make check`, `make test`, and `make hawk`; Hawk requires the prebuilt 0.1.8
release on `PATH` and its pinned Rust 1.97.0 toolchain. Run
`make test-bazel-matrix` for runner, BEP, policy, or reducer changes. Reducer
changes require reviewed fixture/snapshot diffs. Raw fixtures must not contain
usernames, hostnames, credentials, or absolute local paths.

Reducer work should add or update a manifest-driven live example and recorded
contract. Run `make test-reducer-corpus`; use the explicitly gated record and
accept workflow in [the reducer integration testing guide](docs/reducer-integration-testing.md)
for evidence changes. Live Bazel cases must be executed through the MCP harness.

Fuzz with `make fuzz-smoke` or `make fuzz-run FUZZ_TARGET=<name>`. Set up the
explicit Abseil cache with `make setup-oss-corpus`; normal tests never fetch it.

Commits and squash-merge PR titles follow Conventional Commits, for example
`feat(server): add bounded coverage inspection`. Release Please owns versions
and `CHANGELOG.md`; do not edit release versions manually.
