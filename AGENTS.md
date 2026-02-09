# Nova Agent Guide

**This file contains rules ALL agents MUST follow.** Workstream-specific instructions are in `instructions/*.md`.

---

## ⚠️ CRITICAL: Memory Is the Only Hard Constraint

**Before running agents at scale, read the [Operational Guide](docs/00-operational-guide.md).**

When running hundreds of concurrent agents on a shared system (e.g., 192 vCPU / 1.5TB RAM / 110TB Disk):

| Resource | Approach |
|----------|----------|
| **CPU** | Let it burst. Scheduler handles contention fine. Use `-j$(nproc)`. |
| **Disk I/O** | Let it burst. NVMe handles parallel access. |
| **Memory** | **HARD LIMIT via RLIMIT_AS.** Exceeding = process killed (good). |

**Key Rules:**
1. **ALWAYS USE WRAPPER SCRIPTS** - `bash scripts/cargo_agent.sh` for all cargo commands
2. **NEVER RUN UNSCOPED `cargo test`** - Always include `-p/--package <crate>` or `--manifest-path <path>`
   (then optionally further scope with `--lib`, `--bin <name>`, or `--test=<name>`)
3. **GO FAST** - Use all cores for builds. Only memory matters.
4. **FAIL FAST** - Let RLIMIT_AS kill runaway processes, not the whole system.

```bash
# CORRECT:
bash scripts/cargo_agent.sh build --locked --release
bash scripts/cargo_agent.sh test --locked -p nova-core --lib

# WRONG - WILL DESTROY HOST:
cargo test
cargo build --all-targets
```

See [docs/00-operational-guide.md](docs/00-operational-guide.md) for wrapper script details.

---

## Workstreams

Each workstream has its own instruction file in `instructions/`:

| Workstream | File | Key Crates |
|------------|------|------------|
| Core Infrastructure | [`instructions/core-infrastructure.md`](instructions/core-infrastructure.md) | `nova-core`, `nova-db`, `nova-vfs`, `nova-cache`, `nova-memory` |
| Syntax & Parsing | [`instructions/syntax-parsing.md`](instructions/syntax-parsing.md) | `nova-syntax`, `nova-format` |
| Semantic Analysis | [`instructions/semantic-analysis.md`](instructions/semantic-analysis.md) | `nova-types`, `nova-resolve`, `nova-hir`, `nova-flow` |
| Code Intelligence | [`instructions/code-intelligence.md`](instructions/code-intelligence.md) | `nova-ide`, `nova-index`, `nova-fuzzy` |
| Refactoring | [`instructions/refactoring.md`](instructions/refactoring.md) | `nova-refactor` |
| Framework Support | [`instructions/framework-support.md`](instructions/framework-support.md) | `nova-framework-*`, `nova-apt` |
| Build Systems | [`instructions/build-systems.md`](instructions/build-systems.md) | `nova-build`, `nova-build-bazel`, `nova-project` |
| LSP & Editors | [`instructions/lsp-editor.md`](instructions/lsp-editor.md) | `nova-lsp`, `nova-cli`, `editors/*` |
| Debugging | [`instructions/debugging.md`](instructions/debugging.md) | `nova-dap`, `nova-jdwp` |
| AI Features | [`instructions/ai-features.md`](instructions/ai-features.md) | `nova-ai`, `nova-ai-codegen` |
| Testing & Quality | [`instructions/testing-quality.md`](instructions/testing-quality.md) | `nova-testing`, `nova-test-utils`, `fuzz/` |

**Pick your workstream and read its instruction file.** All workstream files require reading this file first.

---

## Project Overview

**Nova** is a next-generation Java Language Server that aims to surpass IntelliJ IDEA. Key innovations:

1. **Query-Based Architecture** - Salsa-inspired incremental computation
2. **Resilient by Design** - Works with broken, incomplete code
3. **Performance as a Feature** - Sub-16ms latency for most operations
4. **Composability** - Library-first design, standard protocols (LSP/DAP)

### Document Structure

| Part | Documents |
|------|-----------|
| **Operations** | [00 - Operational Guide](docs/00-operational-guide.md) (**READ FIRST**) |
| **Problem Space** | [01 - Problem Analysis](docs/01-problem-analysis.md), [02 - Current Landscape](docs/02-current-landscape.md) |
| **Architecture** | [03 - Architecture Overview](docs/03-architecture-overview.md), [04 - Incremental Computation](docs/04-incremental-computation.md), [05 - Syntax](docs/05-syntax-and-parsing.md), [06 - Semantic Analysis](docs/06-semantic-analysis.md), [16 - Java Language Levels](docs/16-java-language-levels.md) |
| **Intelligence** | [07 - Code Intelligence](docs/07-code-intelligence.md), [08 - Refactoring](docs/08-refactoring-engine.md), [09 - Framework Support](docs/09-framework-support.md) |
| **Integration** | [10 - Performance](docs/10-performance-engineering.md), [11 - Editor Integration](docs/11-editor-integration.md), [12 - Debugging](docs/12-debugging-integration.md), [17 - Observability](docs/17-observability-and-reliability.md) |
| **Advanced** | [13 - AI Augmentation](docs/13-ai-augmentation.md), [14 - Testing Strategy](docs/14-testing-strategy.md), [14 - Testing Infrastructure](docs/14-testing-infrastructure.md) |
| **Planning** | [15 - Work Breakdown](docs/15-work-breakdown.md), [Architecture + ADRs](docs/architecture.md) |

---

## Mandatory Rules (All Workstreams)

### Cargo Commands

```bash
# ALWAYS use wrapper:
bash scripts/cargo_agent.sh build --locked --release
bash scripts/cargo_agent.sh test --locked -p nova-core --lib
bash scripts/cargo_agent.sh check --locked -p nova-syntax

# NEVER run these:
cargo test                    # Unbounded
cargo build --all-targets     # Will OOM
cargo check --all-features    # Will OOM
```

### Rust Toolchain (Pinned)

Nova pins the Rust toolchain via [`rust-toolchain.toml`](rust-toolchain.toml) to keep CI and local
builds reproducible (most importantly: `rustfmt` + `clippy` output).

- **Pinned toolchain**: **Rust 1.92.0** (see `rust-toolchain.toml` and `.github/workflows/*.yml`)
- **Wrapper behavior**: `bash scripts/cargo_agent.sh …` **prefers the pinned toolchain**, even if your
  environment sets `RUSTUP_TOOLCHAIN` (common in shared runner / agent environments).

If you need to run with a different toolchain (debugging toolchain regressions / forward-compat only),
pass an explicit rustup toolchain spec as the first argument:

```bash
# Use nightly for workflows that require it (e.g. fuzzing).
bash scripts/cargo_agent.sh +nightly fuzz list

# Debug a toolchain-specific issue (do not use for normal development).
bash scripts/cargo_agent.sh +1.93.0 check --locked -p nova-core
```

### Test Organization

**Avoid creating loose `tests/*.rs` files.** Each `.rs` file in `tests/` becomes a separate binary.
Prefer a single integration test harness per crate; a second harness is allowed only when there is a
strong reason (e.g. CI entrypoints or process-global cache isolation). `nova-devtools` warns at 2
root `tests/*.rs` files and errors at >2.

```
tests/
├── harness.rs      ← harness (compiles as ONE binary)
└── suite/          ← subdirectory
    ├── mod.rs
    └── your_new_test.rs ← ADD HERE
```

### Cross-Platform Compatibility

1. **Path canonicalization**: macOS `/var` → `/private/var`. Canonicalize tempdir paths in tests.
2. **Path separators**: Use `Path::join`, never hardcoded `/`.
3. **Line endings**: Normalize or use `.lines()` for comparison.
4. **Case sensitivity**: Don't rely on case-sensitive paths.

See [Testing Infrastructure](docs/14-testing-infrastructure.md) for details.

### Code Quality

- Fix linter errors before committing
- Run `bash scripts/cargo_agent.sh check --locked -p <crate>` before pushing
- Add tests for new functionality
- Follow existing code conventions in each crate

---

## Architecture Quick Reference

```
┌─────────────────────────────────────────────────────────────────┐
│                    Query Database (nova-db)                      │
├─────────────────────────────────────────────────────────────────┤
│  Input Queries      │  Derived Queries                          │
│  ─────────────────  │  ────────────────────────────────────────  │
│  • file_content     │  • parse_file → Syntax Tree               │
│  • file_exists      │  • resolve_imports → Import Resolution    │
│  • config           │  • type_check → Type Information          │
│                     │  • completions_at → Completion Items      │
│                     │  • diagnostics_for → Error Messages       │
└─────────────────────────────────────────────────────────────────┘

Dependency Flow:
VFS → Parser → HIR → Types/Resolve → IDE Features → LSP
                                   ↘ Refactoring ↗
```

### Core Design Principles

1. **Query-Based**: Everything is a memoized query with dependency tracking
2. **Incremental**: Only recompute what's affected by changes
3. **Resilient**: Handle broken code gracefully, never crash
4. **Parallel**: Independent queries execute concurrently

---

## Success Metrics

| Metric | IntelliJ Baseline | Nova Target |
|--------|-------------------|-------------|
| Completion latency (p95) | ~100ms | <50ms |
| Rename refactoring (1000 usages) | ~2s | <500ms |
| Memory (1M LOC project) | ~4GB | <1.5GB |
| Time to first completion | ~10s | <2s |
| Framework support depth | Best-in-class | Match or exceed |
| Recovery from syntax errors | Good | Excellent |

---

## Getting Help

- **Architecture questions**: Read [docs/03-architecture-overview.md](docs/03-architecture-overview.md)
- **ADRs**: Check [docs/adr/](docs/adr/) for binding decisions
- **Testing**: See [docs/14-testing-infrastructure.md](docs/14-testing-infrastructure.md)
- **Performance**: See [docs/10-performance-engineering.md](docs/10-performance-engineering.md)

---

*Document Version: 2.0*  
*Created: January 2026*
