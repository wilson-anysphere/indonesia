#!/bin/bash
# Check memory usage of all agents
# Usage: check-agent-memory.sh

CGROUP_ROOT="/sys/fs/cgroup/nova-agents"

echo "Agent Memory Usage"
echo "=================="
printf "%-20s %10s %10s %10s %8s\n" "AGENT" "CURRENT" "HIGH" "MAX" "PROCS"
echo ""

TOTAL_CURRENT=0
ACTIVE_COUNT=0

for cgroup in "$CGROUP_ROOT"/agent-*; do
    if [ ! -d "$cgroup" ]; then
        continue
    fi
    
    agent=$(basename "$cgroup")
    
    # Skip if no processes
    procs=$(cat "$cgroup/cgroup.procs" 2>/dev/null | wc -l)
    if [ "$procs" -eq 0 ]; then
        continue
    fi
    
    ACTIVE_COUNT=$((ACTIVE_COUNT + 1))
    
    # Read memory values
    current=$(cat "$cgroup/memory.current" 2>/dev/null || echo 0)
    high=$(cat "$cgroup/memory.high" 2>/dev/null || echo 0)
    max=$(cat "$cgroup/memory.max" 2>/dev/null || echo 0)
    
    TOTAL_CURRENT=$((TOTAL_CURRENT + current))
    
    # Format sizes
    current_mb=$((current / 1024 / 1024))
    high_mb=$((high / 1024 / 1024))
    if [ "$max" = "max" ]; then
        max_mb="unlimited"
    else
        max_mb="$((max / 1024 / 1024))MB"
    fi
    
    printf "%-20s %8dMB %8dMB %10s %8d\n" "$agent" "$current_mb" "$high_mb" "$max_mb" "$procs"
done | sort -t'M' -k2 -rn

echo ""
echo "----------------------------------------"
TOTAL_MB=$((TOTAL_CURRENT / 1024 / 1024))
TOTAL_GB=$((TOTAL_MB / 1024))
echo "Active agents: $ACTIVE_COUNT"
echo "Total agent memory: ${TOTAL_MB}MB (${TOTAL_GB}GB)"
echo ""
echo "System Memory:"
free -h
