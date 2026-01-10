#!/bin/bash
# Emergency: Kill agents to free memory
# Usage: emergency-memory-relief.sh [target-percent]

set -euo pipefail

TARGET_PERCENT="${1:-75}"
CGROUP_ROOT="/sys/fs/cgroup/nova-agents"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

echo "=== EMERGENCY MEMORY RELIEF ==="
echo "Target: Get system memory under ${TARGET_PERCENT}%"
echo ""

get_mem_percent() {
    free | awk '/Mem:/ {printf "%.0f", $3/$2 * 100}'
}

# Get agents sorted by memory usage (highest first)
get_agents_by_memory() {
    for cgroup in "$CGROUP_ROOT"/agent-*; do
        if [ ! -d "$cgroup" ]; then
            continue
        fi
        
        procs=$(cat "$cgroup/cgroup.procs" 2>/dev/null | wc -l)
        if [ "$procs" -eq 0 ]; then
            continue
        fi
        
        current=$(cat "$cgroup/memory.current" 2>/dev/null || echo 0)
        agent=$(basename "$cgroup" | sed 's/^agent-//')
        echo "$current $agent"
    done | sort -rn
}

CURRENT=$(get_mem_percent)
echo "Current memory usage: ${CURRENT}%"

if [ "$CURRENT" -lt "$TARGET_PERCENT" ]; then
    echo "Memory already under target (${TARGET_PERCENT}%), nothing to do"
    exit 0
fi

echo "Need to free memory. Killing agents starting from highest memory usage..."
echo ""

KILLED=0
while read -r mem agent; do
    [ -z "$agent" ] && continue
    
    CURRENT=$(get_mem_percent)
    if [ "$CURRENT" -lt "$TARGET_PERCENT" ]; then
        echo ""
        echo "Target reached (${CURRENT}%)"
        break
    fi
    
    mem_mb=$((mem / 1024 / 1024))
    echo "Killing agent $agent (using ${mem_mb}MB)..."
    "$SCRIPT_DIR/kill-agent.sh" "$agent" --force 2>/dev/null || true
    KILLED=$((KILLED + 1))
    
    sleep 2  # Give system time to reclaim memory
done < <(get_agents_by_memory)

echo ""
echo "========================================="
echo "Killed $KILLED agents"
echo "Final memory usage: $(get_mem_percent)%"
free -h
