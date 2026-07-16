#!/usr/bin/env bash
set -euo pipefail

root=$(git rev-parse --show-toplevel)
bazel=${BAZEL_MCP_BAZEL:-$(command -v bazelisk || command -v bazel)}
versions=${MATRIX_VERSIONS:-"8.4.2 9.1.0"}
source_workspace="$root/crates/bazel-mcp-reducer/tests/fixtures/workspace"
destination="$root/crates/bazel-mcp-reducer/tests/fixtures"
scratch="$root/.cache/bep-golden-generation"
fixture_home="$scratch/home"
fixture_tmp="$scratch/tmp"
fixture_bazelisk="$root/.cache/bazelisk-fixtures"

python3 - "$scratch" <<'PY'
import pathlib, shutil, sys
path = pathlib.Path(sys.argv[1])
if path.exists():
    shutil.rmtree(path)
path.mkdir(parents=True)
PY
mkdir -p "$fixture_home" "$fixture_tmp" "$fixture_bazelisk"

fixture_env() {
  version=$1
  shift
  env -i \
    HOME="$fixture_home" \
    USER=fixture \
    LOGNAME=fixture \
    PATH="/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin" \
    TMPDIR="$fixture_tmp" \
    BAZELISK_HOME="$fixture_bazelisk" \
    USE_BAZEL_VERSION="$version" \
    "$@"
}

run_case() {
  version=$1
  name=$2
  expected=$3
  command=$4
  shift 4
  major=${version%%.*}
  case_root="$scratch/$major/$name"
  workspace="$scratch/$major/workspace"
  output_root="$case_root/output-root"
  raw_bep="$case_root/raw.bep"
  mkdir -p "$case_root" "$output_root"
  set +e
  (
    cd "$workspace"
    fixture_env "$version" "$bazel" --ignore_all_rc_files \
      "--output_user_root=$output_root" "$command" \
      "--build_event_binary_file=$raw_bep" \
      --build_event_publish_all_actions --color=no --curses=no \
      "$@"
  ) >"$case_root/stdout.log" 2>"$case_root/stderr.log"
  status=$?
  set -e
  if [[ "$status" != "$expected" ]]; then
    echo "$version/$name expected exit $expected, observed $status" >&2
    tail -100 "$case_root/stderr.log" >&2
    exit 1
  fi
  canonical_workspace=$(cd "$workspace" && pwd -P)
  canonical_output=$(cd "$output_root" && pwd -P)
  output="$destination/bazel-$major/$name"
  python3 "$root/scripts/fixtures/sanitize-bep-fixture.py" \
    "$raw_bep" "$output.bep" \
    --replace "FIXTURE_ROOT=$scratch" \
    --replace "BAZELISK_ROOT=$fixture_bazelisk" \
    --replace "WORKSPACE=$workspace" \
    --replace "WORKSPACE=$canonical_workspace" \
    --replace "OUTPUT_ROOT=$output_root" \
    --replace "OUTPUT_ROOT=$canonical_output"
  python3 "$root/scripts/fixtures/sanitize-bep-fixture.py" \
    --text "$case_root/stdout.log" "$output.stdout" \
    --replace "FIXTURE_ROOT=$scratch" \
    --replace "BAZELISK_ROOT=$fixture_bazelisk" \
    --replace "WORKSPACE=$workspace" \
    --replace "WORKSPACE=$canonical_workspace" \
    --replace "OUTPUT_ROOT=$output_root" \
    --replace "OUTPUT_ROOT=$canonical_output"
  python3 "$root/scripts/fixtures/sanitize-bep-fixture.py" \
    --text "$case_root/stderr.log" "$output.stderr" \
    --replace "FIXTURE_ROOT=$scratch" \
    --replace "BAZELISK_ROOT=$fixture_bazelisk" \
    --replace "WORKSPACE=$workspace" \
    --replace "WORKSPACE=$canonical_workspace" \
    --replace "OUTPUT_ROOT=$output_root" \
    --replace "OUTPUT_ROOT=$canonical_output"
  printf '%s\n' "$status" >"$output.exit"
  fixture_env "$version" "$bazel" --ignore_all_rc_files \
    "--output_user_root=$output_root" shutdown \
    >/dev/null 2>&1 || true
}

for version in $versions; do
  major=${version%%.*}
  workspace="$scratch/$major/workspace"
  mkdir -p "$scratch/$major"
  cp -R "$source_workspace" "$workspace"
  rm -f /tmp/bazel-mcp-fixture-flaky-marker

  run_case "$version" loading 1 build //missing:target
  run_case "$version" visibility 1 build //consumer:uses_private
  run_case "$version" keep-going-actions 1 build --keep_going \
    //:compile_failure //:custom_failure_one //:custom_failure_two
  run_case "$version" test-outcomes 3 test --keep_going --cache_test_results=no \
    --flaky_test_attempts=2 --test_timeout=1 --test_output=errors \
    //:pass //:fail //:flaky //:sharded //:timeout

  cached_root="$scratch/$major/cached-tests/output-root"
  mkdir -p "$cached_root"
  (
    cd "$workspace"
    fixture_env "$version" "$bazel" --ignore_all_rc_files \
      "--output_user_root=$cached_root" test \
      --color=no --curses=no //:pass //:sharded
  ) >/dev/null 2>&1
  run_case "$version" cached-tests 0 test --test_output=errors //:pass //:sharded
done

echo "generated and redacted BEP goldens under $destination"
