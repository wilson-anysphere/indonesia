#!/bin/bash
# Show status of all agents and system resources
# Usage: agent-status.sh

CGROUP_ROOT="/sys/fs/cgroup/nova-agents"

echo "=== Nova Agent Status ==="
echo "Generated: $(date)"
echo ""

# System overview
echo "System Resources"
echo "----------------"
free -h | head -2
echo ""

# Calculate memory percentage
MEM_PERCENT=$(free | awk '/Mem:/ {printf "%.0f", $3/$2 * 100}')
if [ "$MEM_PERCENT" -gt 95 ]; then
    echo "⚠️  CRITICAL: Memory at ${MEM_PERCENT}% - consider emergency-memory-relief.sh"
elif [ "$MEM_PERCENT" -gt 85 ]; then
    echo "⚠️  WARNING: Memory at ${MEM_PERCENT}% - stop spawning new agents"
else
    echo "✓  Memory at ${MEM_PERCENT}% - OK"
fi
echo ""

echo "Load average: $(cat /proc/loadavg | cut -d' ' -f1-3)"
echo ""

# Count agents
TOTAL=0
ACTIVE=0
TOTAL_MEM=0

for cgroup in "$CGROUP_ROOT"/agent-*; do
    if [ ! -d "$cgroup" ]; then
        continue
    fi
    TOTAL=$((TOTAL + 1))
    procs=$(cat "$cgroup/cgroup.procs" 2>/dev/null | wc -l)
    if [ "$procs" -gt 0 ]; then
        ACTIVE=$((ACTIVE + 1))
        mem=$(cat "$cgroup/memory.current" 2>/dev/null || echo 0)
        TOTAL_MEM=$((TOTAL_MEM + mem))
    fi
done

echo "Agents"
echo "------"
echo "Total cgroups: $TOTAL"
echo "Active agents: $ACTIVE"
echo "Total agent memory: $((TOTAL_MEM / 1024 / 1024))MB"
echo ""

# Memory by agent (top 20)
if [ "$ACTIVE" -gt 0 ]; then
    echo "Top Agents by Memory"
    echo "--------------------"
    printf "%-25s %10s %8s\n" "AGENT" "MEMORY" "PROCS"
    
    for cgroup in "$CGROUP_ROOT"/agent-*; do
        if [ ! -d "$cgroup" ]; then
            continue
        fi
        
        procs=$(cat "$cgroup/cgroup.procs" 2>/dev/null | wc -l)
        if [ "$procs" -eq 0 ]; then
            continue
        fi
        
        agent=$(basename "$cgroup")
        current=$(cat "$cgroup/memory.current" 2>/dev/null || echo 0)
        current_mb=$((current / 1024 / 1024))
        
        printf "%-25s %8dMB %8d\n" "$agent" "$current_mb" "$procs"
    done | sort -t'M' -k2 -rn | head -20
    
    if [ "$ACTIVE" -gt 20 ]; then
        echo ""
        echo "(showing top 20 of $ACTIVE active agents)"
    fi
fi

echo ""
echo "Quick Commands"
echo "--------------"
echo "  Watch memory:    watch -n 5 $(dirname "$0")/check-agent-memory.sh"
echo "  Kill agent:      $(dirname "$0")/kill-agent.sh <agent-id>"
echo "  Emergency kill:  sudo $(dirname "$0")/emergency-memory-relief.sh 75"
