#!/usr/bin/env bash
set -euo pipefail

# Keep editor integrations in lockstep with the workspace version.
node editors/vscode/scripts/sync-version.mjs

