#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cargo build --manifest-path "$root/Cargo.toml" -p bazel-mcp-server
exec cargo run --manifest-path "$root/Cargo.toml" -p bazel-mcp-benchmark --bin token-integration -- "$@"
