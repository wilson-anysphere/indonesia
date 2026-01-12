#!/usr/bin/env bash
set -euo pipefail

# Enforce ADR 0007 crate dependency boundaries.
#
# Usage:
#   ./scripts/check-deps.sh
#
# (Equivalent to `cargo run -p nova-devtools -- check-deps`.)

# Use a template with trailing Xs for portability (BSD `mktemp` requires it).
tmp="$(mktemp -t nova-crate-deps-metadata.XXXXXX)"
trap 'rm -f "$tmp"' EXIT

# Run `cargo metadata` up-front so `nova-devtools` doesn't have to spawn a nested cargo process
# (which can deadlock on Cargo's global locks under `cargo run`).
#
# Use `--locked` so CI + local runs agree on the resolved workspace graph.
cargo metadata --format-version=1 --no-deps --locked >"$tmp"

# Build once, then run the binary directly to avoid repeated `cargo run` overhead.
cargo build -p nova-devtools --locked

target_dir="${CARGO_TARGET_DIR:-target}"
bin="${target_dir}/debug/nova-devtools"
if [[ "${OS:-}" == "Windows_NT" ]]; then
  bin="${bin}.exe"
fi

"${bin}" check-deps --config crate-layers.toml --metadata-path "$tmp"
