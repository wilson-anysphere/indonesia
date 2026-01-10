#!/bin/bash
# Run a command inside an agent's cgroup
# Usage: run-in-cgroup.sh <agent-id> <command...>

set -euo pipefail

AGENT_ID="${1:?Usage: run-in-cgroup.sh <agent-id> <command...>}"
shift

CGROUP_ROOT="/sys/fs/cgroup/nova-agents"
AGENT_CGROUP="$CGROUP_ROOT/agent-$AGENT_ID"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

# Ensure cgroup exists
if [ ! -d "$AGENT_CGROUP" ]; then
    "$SCRIPT_DIR/create-agent-cgroup.sh" "$AGENT_ID"
fi

# Move ourselves into the cgroup
echo $$ > "$AGENT_CGROUP/cgroup.procs"

# Set process-level limits as backup
ulimit -v $((4 * 1024 * 1024)) 2>/dev/null || true  # 4GB virtual memory
ulimit -n 65536 2>/dev/null || true                  # Open files
ulimit -u 512 2>/dev/null || true                    # Max processes

# Execute the command
exec "$@"
