# 00 - Operational Guide: Running Concurrent Coding Agents

[← Back to Main Document](../AGENTS.md)

## Purpose

This guide is for **coding agents developing Nova** (not end-users of Nova). When hundreds of agents work on the codebase simultaneously on a shared machine, we need guardrails to prevent memory exhaustion while letting agents work as fast as possible.

**Target Environment**: Headless Ubuntu Linux x64 (EC2 or similar), no GPU required

Example specs: 192 vCPU, 1.5TB RAM, 110TB NVMe

---

## The One Rule That Matters

```
┌─────────────────────────────────────────────────────────────────┐
│                                                                  │
│   MEMORY IS THE ONLY HARD CONSTRAINT                            │
│                                                                  │
│   CPU: Let it burst. Scheduler handles contention fine.         │
│   Disk I/O: Let it burst. NVMe can handle parallel access.      │
│   Memory: HARD LIMIT. Exceeding = machine death.                │
│                                                                  │
│   Per-agent memory limit: 4GB                                   │
│   System memory threshold: 85% = stop spawning                  │
│   System memory critical: 95% = start killing                   │
│                                                                  │
│   SWAP MUST BE DISABLED for agent processes.                    │
│   Swap + 300 agents = cascading slowdown = brick.               │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

---

## Quick Setup

Run this once on the host machine as root:

```bash
sudo ./scripts/setup-agent-host.sh
```

This script:
- Disables swap (critical!)
- Sets up cgroups v2 for memory isolation
- Creates workspace pool (500 slots)
- Tunes kernel parameters for many processes
- Sets up tmpfs scratch space

---

## Available Scripts

All scripts are in the `scripts/` directory:

| Script | Purpose |
|--------|---------|
| `setup-agent-host.sh` | One-time host setup (run as root) |
| `spawn-agent.sh` | Start an agent with memory isolation |
| `kill-agent.sh` | Stop an agent (graceful or forced) |
| `check-agent-memory.sh` | Show memory usage per agent |
| `agent-status.sh` | Full status overview |
| `watch-memory.sh` | Continuous memory monitoring |
| `emergency-memory-relief.sh` | Kill agents to free memory |
| `create-agent-cgroup.sh` | Low-level cgroup creation |
| `run-in-cgroup.sh` | Run command in agent's cgroup |

### Usage Examples

```bash
# Spawn an agent
./scripts/spawn-agent.sh agent-001 /var/nova-agents/active/ws-001 cargo build

# Check all agent memory
./scripts/check-agent-memory.sh

# Kill an agent gracefully
./scripts/kill-agent.sh agent-001

# Force kill an agent
./scripts/kill-agent.sh agent-001 --force

# Watch memory with auto-kill if critical
./scripts/watch-memory.sh --auto-kill

# Emergency: kill agents until memory < 75%
sudo ./scripts/emergency-memory-relief.sh 75
```

---

## What Agents Should Know

### Go Fast, Watch Memory

- **DO** use `-j$(nproc)` for builds - CPU contention is fine
- **DO** run tests in parallel - the scheduler handles it
- **DO** use all available cores for compilation
- **DON'T** cache unbounded data in memory
- **DON'T** load entire large files into memory when streaming works
- **DON'T** spawn hundreds of child processes that each consume memory

### Memory-Conscious Patterns

```rust
// GOOD: Stream large files
fn process_large_file(path: &Path) -> Result<()> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    for line in reader.lines() {
        process_line(line?)?;
    }
    Ok(())
}

// BAD: Load everything into memory
fn process_large_file(path: &Path) -> Result<()> {
    let content = std::fs::read_to_string(path)?;  // Could be gigabytes!
    for line in content.lines() {
        process_line(line)?;
    }
    Ok(())
}
```

```rust
// GOOD: Bounded cache
let cache: LruCache<Key, Value> = LruCache::new(NonZeroUsize::new(10000).unwrap());

// BAD: Unbounded cache
let cache: HashMap<Key, Value> = HashMap::new();  // Can grow forever
```

### Timeouts

- **30 minute hard limit** per task - your process will receive SIGTERM
- **15 minute soft limit** - you'll receive SIGUSR1 as a warning to wrap up
- Handle these signals gracefully if doing long-running work

### Handling Timeout Signals

```rust
use signal_hook::{iterator::Signals, consts::*};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

fn main() {
    let should_stop = Arc::new(AtomicBool::new(false));
    let stop_clone = should_stop.clone();
    
    // Handle timeout signals
    std::thread::spawn(move || {
        let mut signals = Signals::new(&[SIGUSR1, SIGTERM]).unwrap();
        for sig in signals.forever() {
            match sig {
                SIGUSR1 => {
                    eprintln!("Soft timeout (15min) - wrapping up...");
                    stop_clone.store(true, Ordering::SeqCst);
                }
                SIGTERM => {
                    eprintln!("Hard timeout (30min) - stopping now");
                    std::process::exit(0);
                }
                _ => {}
            }
        }
    });
    
    // Your work loop
    while !should_stop.load(Ordering::SeqCst) {
        do_work();
    }
    
    save_state();  // Clean up before hard timeout
}
```

### Checking Memory Pressure

```rust
fn should_reduce_memory_usage() -> bool {
    // Check cgroup limit (if running in cgroup)
    if let Ok(current) = std::fs::read_to_string("/sys/fs/cgroup/memory.current") {
        if let Ok(max) = std::fs::read_to_string("/sys/fs/cgroup/memory.max") {
            let current: u64 = current.trim().parse().unwrap_or(0);
            let max: u64 = max.trim().parse().unwrap_or(u64::MAX);
            if current > max * 80 / 100 {
                return true;  // Over 80% of our 4GB limit
            }
        }
    }
    false
}
```

---

## Memory Limit Rationale

```
Total RAM:           1,536 GB
─────────────────────────────
System/kernel:          32 GB   (page cache, buffers, kernel)
Safety headroom:       200 GB   (~13%, for spikes)
Available for agents: 1,304 GB
─────────────────────────────
Per agent limit:         4 GB
Max concurrent:        ~325 agents

Recommended max:       300 agents (leaves buffer)
```

### Why 4GB per agent?

- Rust compilation of medium project: 1-2GB
- Running tests with coverage: 1-2GB  
- IDE-like analysis: 500MB-1GB
- Headroom for spikes: 1GB
- Total reasonable max: 4GB

### Why no CPU/IO limits?

The Linux scheduler is excellent at fair CPU sharing. If 300 agents all try to use CPU, each gets ~1/300th. That's fine - work gets done, just slower.

Disk I/O is similar - NVMe handles parallel access well, and the kernel I/O scheduler prevents starvation.

Memory is different. If 300 agents each try to use 10GB, you need 3TB. You have 1.5TB. Machine dies.

---

## Cgroup Configuration Details

Each agent runs in its own cgroup with these settings:

```bash
# Hard memory limit (OOM kill beyond this)
memory.max = 4G

# Soft limit (kernel starts reclaiming here)
memory.high = 3G

# NO SWAP - fail fast, don't slow down
memory.swap.max = 0

# Process limit (prevent fork bombs)
pids.max = 512

# CPU: No limit (let scheduler handle it)
# I/O: No limit (let NVMe handle it)
```

---

## Monitoring Commands

```bash
# Watch all agent memory usage
watch -n 5 ./scripts/check-agent-memory.sh

# Continuous monitoring with auto-kill
./scripts/watch-memory.sh --auto-kill

# Watch system memory
watch -n 1 free -h

# See what's using memory system-wide
htop --sort-key=PERCENT_MEM

# Check specific agent's memory
cat /sys/fs/cgroup/nova-agents/agent-123/memory.current

# Full status report
./scripts/agent-status.sh
```

---

## Emergency Procedures

### Memory Critical (>95%)

```bash
# Auto-kill agents until under 75%
sudo ./scripts/emergency-memory-relief.sh 75
```

### Kill Specific Agent

```bash
# Graceful (SIGTERM, wait 10s, then SIGKILL)
./scripts/kill-agent.sh agent-123

# Immediate (SIGKILL)
./scripts/kill-agent.sh agent-123 --force
```

### Nuclear Option

```bash
# Kill ALL agents immediately
for cg in /sys/fs/cgroup/nova-agents/agent-*; do
    cat "$cg/cgroup.procs" 2>/dev/null | xargs -r kill -9
done
```

### System Unresponsive

If SSH is still working but system is very slow:

```bash
# Check what's consuming memory
ps aux --sort=-%mem | head -20

# Kill largest process
kill -9 $(ps aux --sort=-%mem | head -2 | tail -1 | awk '{print $2}')
```

---

## Summary Table

| Concern | Approach | Reason |
|---------|----------|--------|
| **Memory** | Hard 4GB limit per agent, NO SWAP | Only thing that can brick machine |
| **CPU** | Unlimited (burst freely) | Scheduler handles contention |
| **Disk I/O** | Unlimited (burst freely) | NVMe + I/O scheduler handle it |
| **Timeouts** | 30 min hard, 15 min soft | Prevent runaway tasks |
| **Processes** | 512 max per agent | Prevent fork bombs |

**Go fast. Just don't eat all the RAM.**

---

[← Back to Main Document](../AGENTS.md)
