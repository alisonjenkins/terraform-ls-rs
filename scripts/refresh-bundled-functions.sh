#!/usr/bin/env bash
# Regenerate the bundled Terraform/OpenTofu built-in function signatures.
#
# Usage:
#   scripts/refresh-bundled-functions.sh [tofu|terraform]
#
# Runs inside `nix develop` (which provides opentofu); falls back to
# `nix shell nixpkgs#opentofu -c tofu ...` if the binary is not in PATH.

set -euo pipefail

binary="${1:-tofu}"
out=schemas/functions.opentofu.json.gz

if ! command -v "$binary" >/dev/null 2>&1; then
  echo "'$binary' not found in PATH — fetching via nix shell..." >&2
  nix shell nixpkgs#opentofu -c tofu metadata functions -json | gzip -9 > "$out"
else
  "$binary" metadata functions -json | gzip -9 > "$out"
fi

echo "Wrote $(du -h "$out" | cut -f1) to $out" >&2
