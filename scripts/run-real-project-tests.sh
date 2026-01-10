#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

cd "${ROOT_DIR}"

./scripts/clone-test-projects.sh

echo "==> Running ignored real-project tests"
cargo test -p nova-project --test real_projects -- --include-ignored
cargo test -p nova-cli --test real_projects -- --include-ignored

echo "==> Done"

