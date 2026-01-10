#!/usr/bin/env bash
set -euo pipefail

./scripts/sync-versions.sh

pushd editors/vscode >/dev/null
npm ci
npm run package
popd >/dev/null

