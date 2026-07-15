#!/usr/bin/env bash
set -euo pipefail

project=${1:-abseil-cpp}
if [[ "$project" != "abseil-cpp" ]]; then
  echo "unsupported project: $project" >&2
  exit 2
fi

commit=5650e9cf76d3be4318d5fa3af38ee483ddfd5e4a
tag=20260526.0
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
destination="$root/.cache/corpora/abseil-cpp/$commit"

if [[ ! -d "$destination/.git" ]]; then
  mkdir -p "$(dirname "$destination")"
  git clone --filter=blob:none --no-checkout https://github.com/abseil/abseil-cpp.git "$destination"
  git -C "$destination" fetch --depth=1 origin "refs/tags/$tag"
  git -C "$destination" checkout --detach "$commit"
fi

actual=$(git -C "$destination" rev-parse HEAD)
if [[ "$actual" != "$commit" ]]; then
  echo "corpus mismatch: expected $commit, found $actual" >&2
  exit 1
fi
printf '%s\n' "$destination"

