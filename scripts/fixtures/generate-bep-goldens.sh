#!/usr/bin/env bash
set -euo pipefail

# Compatibility entry point for the former shell-driven fixture generator.
# Live reducer evidence must now be recorded through the MCP case harness.
root=$(git rev-parse --show-toplevel)
case_id=${1:-${REDUCER_CASE:-}}

if [[ -z "$case_id" ]]; then
  echo "usage: $0 <case-id>" >&2
  echo "the legacy matrix generator was replaced by manifest-driven MCP recordings" >&2
  exit 2
fi

cd "$root"
cargo build -p bazel-mcp-server --bin bazel-mcp

arguments=(record --case "$case_id")
if [[ -n "${BAZEL_MCP_BAZEL:-}" ]]; then
  arguments+=(--bazel "$BAZEL_MCP_BAZEL")
fi
if [[ -n "${BAZEL_VERSION:-}" ]]; then
  arguments+=(--bazel-version "$BAZEL_VERSION")
fi

exec cargo run -p bazel-mcp-reducer-cases -- "${arguments[@]}"
