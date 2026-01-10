#!/bin/bash
# Spawn an agent with proper resource isolation
# Usage: spawn-agent.sh <agent-id> <workspace-path> <command...>

set -euo pipefail

AGENT_ID="${1:?Usage: spawn-agent.sh <agent-id> <workspace> <command...>}"
WORKSPACE="${2:?}"
shift 2

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
LOG_DIR="/var/log/nova-agents"
mkdir -p "$LOG_DIR"

# Check system memory before spawning
MEM_PERCENT=$(free | awk '/Mem:/ {printf "%.0f", $3/$2 * 100}')
if [ "$MEM_PERCENT" -gt 85 ]; then
    echo "ERROR: System memory at ${MEM_PERCENT}%, refusing to spawn new agent" >&2
    echo "Run emergency-memory-relief.sh to free up memory" >&2
    exit 1
fi

# Create cgroup
"$SCRIPT_DIR/create-agent-cgroup.sh" "$AGENT_ID" >/dev/null

# Set timeout (30 minutes hard, 15 minutes soft warning)
HARD_TIMEOUT=1800
SOFT_TIMEOUT=900

# Start soft timeout warning in background
(
    sleep $SOFT_TIMEOUT
    CGROUP="/sys/fs/cgroup/nova-agents/agent-$AGENT_ID"
    if [ -d "$CGROUP" ]; then
        for pid in $(cat "$CGROUP/cgroup.procs" 2>/dev/null); do
            kill -USR1 "$pid" 2>/dev/null || true
        done
    fi
) &
SOFT_PID=$!

# Run with hard timeout and cgroup isolation
cd "$WORKSPACE"
timeout --signal=TERM --kill-after=60 $HARD_TIMEOUT \
    "$SCRIPT_DIR/run-in-cgroup.sh" "$AGENT_ID" \
    "$@" \
    > "$LOG_DIR/$AGENT_ID.stdout.log" 2> "$LOG_DIR/$AGENT_ID.stderr.log" &

AGENT_PID=$!
echo "$AGENT_PID" > "$LOG_DIR/$AGENT_ID.pid"

# Clean up soft timeout process when agent exits
(
    wait $AGENT_PID 2>/dev/null
    kill $SOFT_PID 2>/dev/null || true
) &

echo "Spawned agent $AGENT_ID (PID: $AGENT_PID)"
echo "Logs: $LOG_DIR/$AGENT_ID.stdout.log, $LOG_DIR/$AGENT_ID.stderr.log"
echo "Timeout: ${SOFT_TIMEOUT}s soft (SIGUSR1), ${HARD_TIMEOUT}s hard (SIGTERM)"
