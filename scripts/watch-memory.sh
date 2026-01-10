#!/bin/bash
# Continuously watch memory and auto-kill agents if critical
# Usage: watch-memory.sh [--auto-kill]

AUTO_KILL="${1:-}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

echo "=== Memory Watchdog ==="
echo "Press Ctrl+C to stop"
if [ "$AUTO_KILL" = "--auto-kill" ]; then
    echo "Auto-kill ENABLED: Will kill agents if memory > 95%"
fi
echo ""

while true; do
    clear
    echo "=== Nova Memory Watchdog ($(date '+%H:%M:%S')) ==="
    echo ""
    
    # Get memory stats
    MEM_PERCENT=$(free | awk '/Mem:/ {printf "%.0f", $3/$2 * 100}')
    MEM_USED=$(free -h | awk '/Mem:/ {print $3}')
    MEM_TOTAL=$(free -h | awk '/Mem:/ {print $2}')
    
    # Status indicator
    if [ "$MEM_PERCENT" -gt 95 ]; then
        STATUS="🔴 CRITICAL"
    elif [ "$MEM_PERCENT" -gt 85 ]; then
        STATUS="🟡 WARNING"
    else
        STATUS="🟢 OK"
    fi
    
    echo "System Memory: $MEM_USED / $MEM_TOTAL (${MEM_PERCENT}%) $STATUS"
    echo ""
    
    # Count active agents
    CGROUP_ROOT="/sys/fs/cgroup/nova-agents"
    ACTIVE=0
    TOTAL_AGENT_MEM=0
    
    for cgroup in "$CGROUP_ROOT"/agent-* 2>/dev/null; do
        if [ ! -d "$cgroup" ]; then
            continue
        fi
        procs=$(cat "$cgroup/cgroup.procs" 2>/dev/null | wc -l)
        if [ "$procs" -gt 0 ]; then
            ACTIVE=$((ACTIVE + 1))
            mem=$(cat "$cgroup/memory.current" 2>/dev/null || echo 0)
            TOTAL_AGENT_MEM=$((TOTAL_AGENT_MEM + mem))
        fi
    done
    
    TOTAL_AGENT_GB=$((TOTAL_AGENT_MEM / 1024 / 1024 / 1024))
    echo "Active agents: $ACTIVE (using ~${TOTAL_AGENT_GB}GB)"
    echo ""
    
    # Top 5 agents by memory
    if [ "$ACTIVE" -gt 0 ]; then
        echo "Top 5 agents by memory:"
        for cgroup in "$CGROUP_ROOT"/agent-*; do
            if [ ! -d "$cgroup" ]; then
                continue
            fi
            procs=$(cat "$cgroup/cgroup.procs" 2>/dev/null | wc -l)
            if [ "$procs" -eq 0 ]; then
                continue
            fi
            current=$(cat "$cgroup/memory.current" 2>/dev/null || echo 0)
            agent=$(basename "$cgroup")
            echo "$current $agent"
        done | sort -rn | head -5 | while read -r mem agent; do
            mem_mb=$((mem / 1024 / 1024))
            printf "  %-25s %6dMB\n" "$agent" "$mem_mb"
        done
    fi
    
    # Auto-kill if critical
    if [ "$AUTO_KILL" = "--auto-kill" ] && [ "$MEM_PERCENT" -gt 95 ]; then
        echo ""
        echo "⚠️  AUTO-KILL TRIGGERED (memory > 95%)"
        "$SCRIPT_DIR/emergency-memory-relief.sh" 85
    fi
    
    sleep 5
done
