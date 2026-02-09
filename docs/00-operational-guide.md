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
└─────────────────────────────────────────────────────────────────┘
```

---

## How We Enforce Memory Limits

We use **RLIMIT_AS (address space limit)** via `prlimit` or `ulimit`. This is:
- A **hard kernel limit** - works immediately, no setup required
- **Inherited by child processes** - cargo, rustc, tests all respect it
- **Simple and robust** - no cgroups, no systemd, no root required

Nova’s internal memory budgeting (`nova-memory`) also respects `RLIMIT_AS` when present:
`MemoryBudget::default_for_system()` budgets against the minimum of host RAM, Linux cgroup limit
(when available), and `RLIMIT_AS`. This helps eviction/degraded mode engage before the process hits
the OS-enforced ceiling.

### The Wrapper Scripts

**Always use these wrappers. Never run cargo directly.**

| Script | Purpose |
|--------|---------|
| `scripts/cargo_agent.sh` | Run any cargo command with memory cap |
| `scripts/run_limited.sh` | Run any command with memory/cpu limits |

### Usage

```bash
# CORRECT - Always use the wrapper:
bash scripts/cargo_agent.sh build --locked --release
bash scripts/cargo_agent.sh test --locked -p nova-core --lib
bash scripts/cargo_agent.sh check --locked -p nova-syntax

# WRONG - Will OOM the host:
cargo test                    # Spawns 100s of rustc processes
cargo build --all-targets     # Compiles everything
cargo check --all-features    # Unbounded work
```

### Rust toolchain selection (pinned)

Nova pins its Rust toolchain to keep CI/agent/dev builds reproducible and to avoid `rustfmt`/`clippy`
drift.

- **Pinned toolchain**: **Rust 1.92.0** (see [`rust-toolchain.toml`](../rust-toolchain.toml); CI also
  installs the same version explicitly).
- **Wrapper behavior**: `scripts/cargo_agent.sh` is designed to **prefer the pinned toolchain**, even
  when an external environment sets `RUSTUP_TOOLCHAIN` (common in shared runners).

To use a different toolchain (debugging only), pass an explicit toolchain spec as the first
argument:

```bash
# Example: nightly (required for some workflows like fuzzing)
bash scripts/cargo_agent.sh +nightly fuzz list

# Example: debug a toolchain-specific issue
bash scripts/cargo_agent.sh +1.93.0 check --locked -p nova-core
```

---

## Mandatory Rules for Cargo Commands

### FORBIDDEN (no exceptions):

- `cargo build` / `cargo test` / `cargo check` **without wrapper scripts**
- `cargo test` **without package scoping** (`-p/--package <crate>` or `--manifest-path <path>`)
- `cargo build --all-targets`
- `cargo test --all-targets`
- `cargo check --all-features --tests`
- **ANY command that compiles all targets**

### MANDATORY:

- **Always use `bash scripts/cargo_agent.sh`** for all cargo commands
- **Always scope test runs to a package**: `-p/--package <crate>` or `--manifest-path <path>`
  - Consider further scoping with `--test=<name>`, `--lib`, or `--bin <name>`

```bash
# CORRECT:
bash scripts/cargo_agent.sh build --locked --release
bash scripts/cargo_agent.sh test --locked -p nova-core --lib
bash scripts/cargo_agent.sh test --locked -p nova-format --test harness suite::format_fixtures
bash scripts/cargo_agent.sh check --locked -p nova-syntax

# WRONG — WILL DESTROY HOST:
cargo test
cargo build --all-targets
cargo check --all-features --tests
```

**Why this matters**: If you run unscoped cargo commands, you will compile dozens of binaries with LTO, spawn hundreds of parallel rustc/linker processes, exhaust all RAM, and render the machine unusable. **There are no exceptions.**

---

## Script Details

### `scripts/run_limited.sh`

Runs any command under OS-enforced resource limits. Prefers `prlimit` (hard limits), falls back to `ulimit`.

```bash
# Run with 4GB address space limit (default)
bash scripts/run_limited.sh -- ./my-program

# Override the limit
bash scripts/run_limited.sh --as 8G -- ./memory-hungry-program

# Multiple limits
bash scripts/run_limited.sh --as 8G --cpu 3600 -- ./long-running-task
```

Options:
- `--as <size>`: Address-space limit (e.g., `4G`, `8192M`). Default: `4G`
- `--cpu <secs>`: CPU time limit in seconds
- `--stack <size>`: Stack size limit

### `scripts/cargo_agent.sh`

High-throughput cargo wrapper for multi-agent hosts.

```bash
bash scripts/cargo_agent.sh build --locked --release
bash scripts/cargo_agent.sh test --locked -p nova-core --lib
bash scripts/cargo_agent.sh check --locked -p nova-syntax --quiet
```

Features:
- Enforces RAM cap via `RLIMIT_AS` (default: `4G`)
- Throttles concurrent cargo commands with slot locks
- Caps `RUST_TEST_THREADS` to avoid spawning hundreds of test threads
- Uses the repo-pinned Rust toolchain (`rust-toolchain.toml`, **1.92.0**) for reproducible builds.
- Supports explicit toolchain overrides via rustup syntax (`+nightly`, `+1.93.0`, etc.)

Environment variables:
- `NOVA_CARGO_LIMIT_AS`: Address-space cap (default: `4G`)
- `NOVA_CARGO_SLOTS`: Max concurrent cargo commands (default: auto)
- `NOVA_CARGO_JOBS`: Force `-j` value (default: cargo's default)
- `NOVA_RUST_TEST_THREADS`: Test thread cap (default: `min(nproc, 8)`)
- `NOVA_CARGO_ALLOW_UNSCOPED_TEST`: Set to `1` to allow unscoped `cargo test` (not recommended; prefer `-p/--package` or `--manifest-path`)

---

## Why RLIMIT_AS, Not Cgroups?

1. **No setup required** - Works on any Linux system immediately
2. **No root required** - Any user can set their own limits
3. **Simpler to understand** - One number, hard limit
4. **More reliable** - Kernel enforces it, no daemon needed
5. **Inherited automatically** - All child processes get the same limit

Cgroups are more powerful (can limit RSS, I/O, CPU shares), but:
- Require host configuration
- Need root to set up
- More complex to manage
- Overkill when address-space limits solve the problem

**Address space != RSS**: A process can reserve 64GB of address space but only use 1GB of physical RAM. This is fine - RLIMIT_AS prevents runaway allocations, and the kernel handles actual memory pressure.

---

## Memory Budget Rationale

```
Total RAM:           1,536 GB
─────────────────────────────
System/kernel:          32 GB
Safety headroom:       200 GB
Available for agents: 1,304 GB
─────────────────────────────
Per agent limit:         4 GB
Max concurrent:        ~325 agents

Recommended max:       300 agents
```

### Why 4GB per agent?

- Rust compilation of medium crate: 1-2GB
- Running tests: 500MB-1GB
- Linker (especially with LTO): 1-2GB
- Headroom for spikes: 1GB
- Total reasonable max: 4GB

---

## Tuning Nova's internal memory budget (optional)

`RLIMIT_AS` limits the **process** address space, but Nova also uses a cooperative in-process cache
budget (`nova-memory`) to stay within predictable memory bounds when running many concurrent
servers/agents.

You can override the default cache budget without code changes:

```bash
# Limit Nova's internal caches to 1GiB total (split across categories by default).
export NOVA_MEMORY_BUDGET_TOTAL=1G
```

You can also set per-category budgets (`NOVA_MEMORY_BUDGET_QUERY_CACHE`, `..._SYNTAX_TREES`, etc) or
use a workspace `nova.toml` `[memory]` table. Environment variables take precedence over config.

---

## Handling OOMs

When RLIMIT_AS is hit, the process gets a memory allocation failure (usually manifests as `out of memory` or similar error). This is **expected behavior** - it's the limit working.

If this happens:
1. **Good**: The limit protected the system
2. **Fix**: Either the task needs more memory (bump limit) or the code has a memory bug

```bash
# If 4GB isn't enough for a specific task:
NOVA_CARGO_LIMIT_AS=8G bash scripts/cargo_agent.sh build --locked --release

# Or use run_limited directly:
bash scripts/run_limited.sh --as 8G -- ./memory-hungry-task
```

---

## Disk Hygiene

`target/` grows without bound. Check size periodically and clean when over budget:

```bash
TARGET_MAX_GB="${TARGET_MAX_GB:-100}"
TARGET_MAX_BYTES=$((TARGET_MAX_GB * 1024 * 1024 * 1024))

if [[ -d target ]]; then
  size_bytes=$(du -sb target 2>/dev/null | cut -f1 || echo 0)
  if [[ "${size_bytes}" -ge "${TARGET_MAX_BYTES}" ]]; then
    echo "target/ exceeds ${TARGET_MAX_GB}GB; running cargo clean..."
    bash scripts/cargo_agent.sh clean --locked
  fi
fi
```

---

## Test Organization

When this repo grows to have many test targets, **never create loose `tests/*.rs` files**. Each `.rs` file directly in `tests/` becomes a separate binary that must be compiled and linked.

Add tests to existing harness subdirectories:

```
tests/
├── parser_tests.rs      ← harness (compiles as ONE binary)
├── parser/              ← subdirectory
│   ├── mod.rs
│   └── your_new_test.rs ← ADD YOUR TEST HERE
├── semantic_tests.rs
├── semantic/
└── ...
```

---

## AI audit log permissions (Unix)

When `ai.audit_log.enabled=true`, Nova can write AI audit events (prompts / model output) to a
separate file. On Unix-like systems this audit log file is created with permissions `0600` (owner
read/write only). If the file already exists with broader permissions, Nova emits a warning.

---

## Go Fast

Remember: **CPU and I/O are free**. Use them aggressively.

```bash
# GOOD: Use all cores
bash scripts/cargo_agent.sh build --locked --release -j$(nproc)

# GOOD: Run tests in parallel
bash scripts/cargo_agent.sh test --locked -p nova-core --lib

# The wrapper handles throttling - you don't need to be conservative
```

The only constraint is memory. The wrapper enforces it. Go fast.

---

[← Back to Main Document](../AGENTS.md)
