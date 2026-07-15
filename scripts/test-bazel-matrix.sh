#!/usr/bin/env bash
set -euo pipefail

versions=${MATRIX_VERSIONS:-"7.6.1 8.4.2 9.1.0"}
server=${BAZEL_MCP_SERVER_BIN:-"$PWD/target/debug/bazel-mcp"}
bazel=${BAZEL_MCP_BAZEL:-$(command -v bazelisk || command -v bazel)}
mkdir -p .cache/bazel-matrix
cargo build -p bazel-mcp-server --bin bazel-mcp
printf 'version\tcase\tstate\texit\n'
for version in $versions; do
  workspace="$PWD/.cache/bazel-matrix/$version/workspace"
  rm -rf "$workspace"
  mkdir -p "$workspace"
  cp -R crates/bazel-mcp-runner/tests/workspaces/basic/. "$workspace/"
  export USE_BAZEL_VERSION="$version"
  while IFS= read -r row; do
    printf '%s\t%s\n' "$version" "$row"
  done < <(python3 scripts/test-mcp-smoke.py \
    --workspace "$workspace" --server "$server" --bazel "$bazel" \
    --root "$PWD/.cache/bazel-matrix/$version/runtime")
done
