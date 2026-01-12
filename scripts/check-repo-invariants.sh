#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT_DIR}"

cargo_agent() {
  bash "${ROOT_DIR}/scripts/cargo_agent.sh" "$@"
}

# Run Nova repository invariants enforced by `nova-devtools`.
#
# This is the local/dev convenience equivalent of the CI "repo invariants" step.
#
# Usage:
#   ./scripts/check-repo-invariants.sh

# Some environments configure a global rustc wrapper (commonly `sccache`) via cargo config.
# This can be flaky in multi-agent sandboxes. Mirror `scripts/cargo_agent.sh` and disable
# rustc wrappers by default for reliability; callers that want to keep them can set
# `NOVA_CARGO_KEEP_RUSTC_WRAPPER=1`.
if [[ -z "${NOVA_CARGO_KEEP_RUSTC_WRAPPER:-}" ]]; then
  export RUSTC_WRAPPER=""
  export RUSTC_WORKSPACE_WRAPPER=""
fi

# Use a template with trailing Xs for portability (BSD `mktemp` requires it).
tmp="$(mktemp -t nova-devtools-metadata.XXXXXX)"
trap 'rm -f "$tmp"' EXIT

# Generate metadata once and reuse it across all checks.
cargo_agent metadata --format-version=1 --no-deps --locked >"$tmp"

# Build once, then run the binary directly to avoid repeated `cargo run` overhead in CI.
cargo_agent build -p nova-devtools --locked

target_dir="${CARGO_TARGET_DIR:-target}"
bin="${target_dir}/debug/nova-devtools"
if [[ "${OS:-}" == "Windows_NT" ]]; then
  bin="${bin}.exe"
fi

"${bin}" check-repo-invariants --metadata-path "$tmp"
