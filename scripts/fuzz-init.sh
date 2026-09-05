#!/usr/bin/env bash
set -euo pipefail
mkdir -p fuzz/corpus fuzz/artifacts
rustup run "$(cat fuzz/rust-toolchain)" cargo fuzz list >/dev/null
