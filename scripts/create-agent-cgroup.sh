#!/bin/bash
# Create a cgroup for a specific agent with memory limits
# Usage: create-agent-cgroup.sh <agent-id>

set -euo pipefail

AGENT_ID="${1:?Usage: create-agent-cgroup.sh <agent-id>}"
CGROUP_ROOT="/sys/fs/cgroup/nova-agents"
AGENT_CGROUP="$CGROUP_ROOT/agent-$AGENT_ID"

# Create cgroup if it doesn't exist
mkdir -p "$AGENT_CGROUP"

# Memory limits (THE CRITICAL ONES)
echo "4G" > "$AGENT_CGROUP/memory.max"          # Hard limit: 4GB
echo "3G" > "$AGENT_CGROUP/memory.high"         # Soft limit: triggers reclaim at 3GB
echo "0" > "$AGENT_CGROUP/memory.swap.max"      # NO SWAP - fail fast, don't brick machine

# Process limit (prevent fork bombs, but generous)
echo "512" > "$AGENT_CGROUP/pids.max"

# Note: We intentionally don't limit CPU or I/O
# Let agents use full resources - OS scheduler handles contention fine
# Only memory can brick the machine

echo "$AGENT_CGROUP"
