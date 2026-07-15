#!/usr/bin/env bash
set -euo pipefail
mkdir -p fuzz/corpus fuzz/artifacts
cargo +nightly fuzz list >/dev/null
