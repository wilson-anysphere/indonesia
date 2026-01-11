#!/usr/bin/env bash
set -euo pipefail

# Enforce ADR 0007 crate dependency boundaries.
#
# Usage:
#   ./scripts/check-deps.sh
#
# (Equivalent to `cargo run -p nova-devtools -- check-deps`.)

cargo run --locked -p nova-devtools -- check-deps --config crate-layers.toml

