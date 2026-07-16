#!/usr/bin/env bash
set -euo pipefail

versions=${MATRIX_VERSIONS:-"8.4.2 9.1.0"}
server=${BAZEL_MCP_SERVER_BIN:-"$PWD/target/debug/bazel-mcp"}
bazel=${BAZEL_MCP_BAZEL:-$(command -v bazelisk || command -v bazel)}
run_id=${BAZEL_MATRIX_RUN_ID:-"$(date +%s)-$$"}
matrix_root=${BAZEL_MATRIX_ROOT:-"${TMPDIR:-/tmp}/bazel-mcp-matrix/$run_id"}
mkdir -p "$matrix_root"
cargo build -p bazel-mcp-server --bin bazel-mcp
printf 'version\tcase\tstate\texit\n'
for version in $versions; do
  workspace="$matrix_root/$version/workspace"
  mkdir -p "$workspace"
  cp -R crates/bazel-mcp-runner/tests/workspaces/basic/. "$workspace/"
  mkdir -p "$workspace/tools"
  cat > "$workspace/tools/bazel" <<'EOF'
#!/usr/bin/env sh
exec "$BAZEL_MCP_WRAPPED_BAZEL" "$@"
EOF
  chmod 700 "$workspace/tools/bazel"
  export USE_BAZEL_VERSION="$version"
  export BAZEL_MCP_WRAPPED_BAZEL="$bazel"
  case_output="$matrix_root/$version/cases.tsv"
  python3 scripts/test-mcp-smoke.py \
    --workspace "$workspace" --server "$server" --bazel "$bazel" \
    --root "$matrix_root/$version/runtime" --wrapper >"$case_output"
  while IFS= read -r row; do
    printf '%s\t%s\n' "$version" "$row"
  done <"$case_output"
done
