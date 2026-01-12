#!/usr/bin/env bash
set -euo pipefail

# Run Nova repository invariants enforced by `nova-devtools`.
#
# This is the local/dev convenience equivalent of the CI "repo invariants" step.
#
# Usage:
#   ./scripts/check-repo-invariants.sh

# Use a template with trailing Xs for portability (BSD `mktemp` requires it).
tmp="$(mktemp -t nova-devtools-metadata.XXXXXX)"
trap 'rm -f "$tmp"' EXIT

# Generate metadata once and reuse it across all checks.
cargo metadata --format-version=1 --no-deps --locked >"$tmp"

# Build once, then run the binary directly to avoid repeated `cargo run` overhead in CI.
cargo build -p nova-devtools --locked

target_dir="${CARGO_TARGET_DIR:-target}"
bin="${target_dir}/debug/nova-devtools"
if [[ "${OS:-}" == "Windows_NT" ]]; then
  bin="${bin}.exe"
fi

"${bin}" check-deps --metadata-path "$tmp"
"${bin}" check-layers --metadata-path "$tmp"
"${bin}" check-architecture-map --metadata-path "$tmp" --strict
"${bin}" check-protocol-extensions
