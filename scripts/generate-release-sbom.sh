#!/usr/bin/env bash
set -euo pipefail
if ! command -v cargo-cyclonedx >/dev/null 2>&1; then
  echo 'cargo-cyclonedx is required' >&2
  exit 1
fi
cargo cyclonedx \
  --manifest-path crates/bazel-mcp-server/Cargo.toml \
  --format json \
  --override-filename bazel-mcp.cdx
mkdir -p target/distrib
mv crates/bazel-mcp-server/bazel-mcp.cdx.json target/distrib/bazel-mcp.cdx.json
find crates -name 'bazel-mcp.cdx.json' -delete
