#!/usr/bin/env bash
set -euo pipefail
for _ in $(seq 1 2000); do
  echo 'warning: duplicated benchmark warning' >&2
done
echo 'ERROR: BAZEL_MCP_NOISY_ROOT_CAUSE: deterministic custom action failure' >&2
exit 1

