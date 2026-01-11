#!/usr/bin/env bash
set -euo pipefail

# Run Nova repository invariants enforced by `nova-devtools`.
#
# This is the local/dev convenience equivalent of the CI "repo invariants" step.
#
# Usage:
#   ./scripts/check-repo-invariants.sh

tmp="$(mktemp -t nova-devtools-metadata.XXXXXX.json)"
trap 'rm -f "$tmp"' EXIT

# Generate metadata once and reuse it across all checks.
cargo metadata --format-version=1 --no-deps --locked >"$tmp"

cargo run -p nova-devtools -- check-deps --metadata-path "$tmp"
cargo run -p nova-devtools -- check-layers --metadata-path "$tmp"
cargo run -p nova-devtools -- check-architecture-map --metadata-path "$tmp" --strict

