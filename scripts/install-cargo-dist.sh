#!/usr/bin/env bash
set -euo pipefail

readonly DIST_VERSION="0.31.0"
readonly INSTALLER_SHA256="e79d87e418b9d2cbe992d014985457c28a5a7c553add3da4ed1047e161c928f4"
readonly INSTALLER_URL="https://github.com/axodotdev/cargo-dist/releases/download/v${DIST_VERSION}/cargo-dist-installer.sh"

installer="$(mktemp)"
trap 'rm -f "$installer"' EXIT

curl --proto '=https' --tlsv1.2 --location --silent --show-error --fail \
  --output "$installer" "$INSTALLER_URL"
echo "${INSTALLER_SHA256}  ${installer}" | shasum -a 256 --check
sh "$installer"
