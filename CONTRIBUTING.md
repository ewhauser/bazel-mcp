# Contributing

Install the pinned Rust toolchain, Bazelisk, Git, a C++ toolchain, and the tools
provided by `nix develop`. Run `make setup-hooks` once per checkout.

Use the root Makefile as the stable interface. Before opening a pull request,
run `make check` and `make test`; run `make test-bazel-matrix` for runner, BEP,
policy, or reducer changes. Reducer changes require reviewed fixture/snapshot
diffs. Raw fixtures must not contain usernames, hostnames, credentials, or
absolute local paths.

Fuzz with `make fuzz-smoke` or `make fuzz-run FUZZ_TARGET=<name>`. Set up the
explicit Abseil cache with `make setup-oss-corpus`; normal tests never fetch it.

Commits and squash-merge PR titles follow Conventional Commits, for example
`feat(server): add bounded coverage inspection`. Release Please owns versions
and `CHANGELOG.md`; do not edit release versions manually.
