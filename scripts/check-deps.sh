#!/usr/bin/env bash
set -euo pipefail

# Enforce ADR 0007 crate dependency boundaries.
#
# Usage:
#   ./scripts/check-deps.sh
#
# (Equivalent to `cargo run -p nova-devtools -- check-deps`.)

tmp="$(mktemp -t nova-crate-deps-metadata.XXXXXX.json)"
trap 'rm -f "$tmp"' EXIT

# Run `cargo metadata` up-front (outside of `cargo run`) so `nova-devtools` doesn't have to spawn a
# nested cargo process (which can deadlock on Cargo's global locks).
cargo metadata --format-version=1 --no-deps >"$tmp"

cargo run -p nova-devtools -- check-deps --config crate-layers.toml --metadata-path "$tmp"
