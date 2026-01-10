#!/bin/bash
# Kill an agent (gracefully or forcefully)
# Usage: kill-agent.sh <agent-id> [--force]

set -euo pipefail

AGENT_ID="${1:?Usage: kill-agent.sh <agent-id> [--force]}"
FORCE="${2:-}"

CGROUP_ROOT="/sys/fs/cgroup/nova-agents"
AGENT_CGROUP="$CGROUP_ROOT/agent-$AGENT_ID"
LOG_DIR="/var/log/nova-agents"
PID_FILE="$LOG_DIR/$AGENT_ID.pid"

if [ ! -d "$AGENT_CGROUP" ]; then
    echo "Agent $AGENT_ID not found (no cgroup)"
    exit 0
fi

# Get all PIDs in the cgroup
PIDS=$(cat "$AGENT_CGROUP/cgroup.procs" 2>/dev/null || true)

if [ -z "$PIDS" ]; then
    echo "No processes found for agent $AGENT_ID, cleaning up cgroup"
    rmdir "$AGENT_CGROUP" 2>/dev/null || true
    rm -f "$PID_FILE"
    exit 0
fi

if [ "$FORCE" = "--force" ] || [ "$FORCE" = "-f" ]; then
    echo "Force killing agent $AGENT_ID..."
    for pid in $PIDS; do
        kill -9 "$pid" 2>/dev/null || true
    done
else
    echo "Gracefully stopping agent $AGENT_ID (SIGTERM)..."
    for pid in $PIDS; do
        kill -TERM "$pid" 2>/dev/null || true
    done
    
    # Wait up to 10 seconds for graceful shutdown
    for i in {1..10}; do
        PIDS=$(cat "$AGENT_CGROUP/cgroup.procs" 2>/dev/null || true)
        if [ -z "$PIDS" ]; then
            break
        fi
        echo "  Waiting... ($i/10)"
        sleep 1
    done
    
    # Force kill any remaining
    PIDS=$(cat "$AGENT_CGROUP/cgroup.procs" 2>/dev/null || true)
    if [ -n "$PIDS" ]; then
        echo "Force killing remaining processes..."
        for pid in $PIDS; do
            kill -9 "$pid" 2>/dev/null || true
        done
    fi
fi

# Clean up
sleep 1
rmdir "$AGENT_CGROUP" 2>/dev/null || true
rm -f "$PID_FILE"

echo "Agent $AGENT_ID killed"
