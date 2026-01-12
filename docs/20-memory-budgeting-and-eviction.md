# 20 - Memory Budgeting and Eviction (Core Infra Integration Plan)

This document is a **sub-planning track** for end-to-end memory budgeting + eviction across Nova’s
core infrastructure crates, with the explicit goal of **remaining responsive** (no panics, no
runaway OOM thrash) when Nova is under memory pressure.

Scope focus:

- `crates/nova-memory` (budgets, pressure detection, eviction API, orchestration)
- `crates/nova-db` (Salsa memo eviction, persistence flush hooks, query caches)
- `crates/nova-workspace` (open-doc pinning; long-lived caches held outside Salsa)
- `crates/nova-index` (index memory tracking/estimation; long-lived index caches)
- Dependency indexes / caches that live “forever” in a session (classpath + JDK indexes; `nova-cache`)

---

## 1) Major memory consumers in a large workspace

In practice, Nova’s RSS in a large workspace is dominated by a few “shape classes” of memory:

1. **Salsa memo tables** (incremental query cache)
   - Many `Arc<T>` values live in `ra_salsa::Storage` memo tables.
   - Includes parse trees, HIR/item trees, semantic summaries, typechecking artifacts.
2. **Syntax trees**
   - Long-lived parse results (Rowan green trees + auxiliary structures).
   - Often proportional to total source text size of the working set.
3. **Workspace indexes**
   - `nova-index::ProjectIndexes` (symbols, references, inheritance, annotations).
   - Workspace-wide fuzzy symbol search indices (`TrigramIndex` etc).
4. **Classpath + dependency indexes**
   - `nova_classpath::ClasspathIndex` and/or `nova_deps_cache` bundles.
   - Can be very large for projects with many JARs.
5. **JDK index**
   - `nova_jdk::JdkIndex` caches (JMOD symbol caches, stub maps).
6. **Open-document overlays**
   - In-memory document text (and any derived per-open-doc data).
   - Must remain available for UX correctness (the editor’s current buffers).
7. **“Other” long-lived caches**
   - Various LRU caches, memoization helpers, per-session registries, extension state, etc.

Important nuance: some “big” datasets are **mmap-backed** (e.g. `nova-storage::PersistedArchive`)
and show up in **RSS** while being mostly **OS page-cache managed**. We still need to respect RSS,
because that’s what the kernel will kill under container limits.

---

## 2) What is tracked today vs untracked? Where are the gaps?

Nova already has the right *shape* of a memory system (`nova-memory`), but integration is partial.

### Existing building blocks

- `nova_memory::MemoryManager`:
  - Budget split into coarse categories (`QueryCache`, `SyntaxTrees`, `Indexes`, `TypeInfo`, `Other`)
  - Pressure computed from `max(tracked_total, process_rss)` (`crates/nova-memory/src/manager.rs`)
  - Under `High`/`Critical`, calls `MemoryEvictor::flush_to_disk()` best-effort, then evicts
    proportionally.
- `nova_memory::MemoryEvictor`:
  - Cooperative eviction; must remain snapshot-safe (cached values should be behind `Arc`)

### Currently tracked + evictable

| Component | Crate | Category | Tracked? | Evictable? | Notes |
|---|---|---:|---:|---:|---|
| `QueryCache` (hot/warm + optional disk spill) | `nova-db` | QueryCache | yes | yes | `flush_to_disk()` persists warm tier (`crates/nova-db/src/query_cache.rs`) |
| Salsa memos (coarse footprint) | `nova-db` | QueryCache | partial | yes | `SalsaMemoEvictor` rebuilds DB to drop memos (`crates/nova-db/src/salsa/mod.rs`) |
| `SyntaxTreeStore` | `nova-syntax` | SyntaxTrees | yes | yes | Pins open docs, evicts closed files first (`crates/nova-syntax/src/tree_store.rs`) |
| `IndexCache` (generic bytes) | `nova-index` | Indexes | yes | yes | LRU-based (`crates/nova-index/src/memory_cache.rs`) |
| `WorkspaceSymbolSearcher` | `nova-index` | Indexes | yes | yes | Tracks trigram/prefix index bytes (`crates/nova-index/src/symbol_search.rs`) |

### Currently tracked but non-evictable

| Component | Crate | Category | Tracked? | Notes |
|---|---|---:|---:|---|
| Open document text (editor buffers) | `nova-lsp` | Other | yes | Tracked via `documents_memory` registration (`crates/nova-lsp/src/main.rs`) |

### Major gaps (high impact)

1. **Workspace `ProjectIndexes` held in memory are not tracked and not evictable**
   - `crates/nova-workspace/src/engine.rs`: `indexes: Arc<Mutex<ProjectIndexes>>` (unregistered).
2. **Classpath index is not tracked**
   - `nova-workspace` constructs `nova_classpath::ClasspathIndex` and stores it as a Salsa input;
     no memory tracking / eviction hook exists.
3. **JDK index is not tracked**
   - LSP keeps `ServerState.jdk_index: Option<nova_jdk::JdkIndex>` outside `nova-memory`.
4. **Type info category is unused**
   - No component currently registers under `MemoryCategory::TypeInfo`.
   - Many of the “real” heavyweight Salsa outputs (HIR/typeck/etc.) are not accounted separately.
5. **VFS overlay memory is only partially tracked**
   - `nova-lsp` tracks open-document *text* sizes, but overlay documents opened outside the “open
     docs” set (e.g. virtual/decompiled files opened via `overlay().open(...)`) are not counted.

These gaps matter because `MemoryManager` uses RSS as an upper bound for *pressure*, but it can
only evict *tracked + evictable* components. If RSS is dominated by untracked memory, Nova will
enter `High/Critical` pressure and **stay there** (degraded UX) without being able to recover.

---

## 3) What should eviction do at each pressure level?

The system already defines 4 pressure levels (`Low/Medium/High/Critical`) and “degraded settings”
(`crates/nova-memory/src/degraded.rs`). The missing piece is making eviction behavior consistent
across components and ensuring we evict the *right* things first.

### Proposed policy: “cheap-first, pinned-first, persistent-first”

1. **Cheap-first:** evict caches that are cheap to rebuild before expensive ones.
2. **Pinned-first:** never evict open-document *inputs*; prefer evicting closed-file artifacts.
3. **Persistent-first:** under High/Critical, persist cold artifacts before dropping them.

### Pressure → actions (component-level)

| Pressure | Query caches (QueryCache + Salsa memos) | Syntax trees | Indexes | Classpath/JDK | Background work |
|---|---|---|---|---|---|
| Low | Enforce per-category budget; LRU shrink. Avoid expensive global clears. | Drop closed-file trees only. | Drop cold caches only. | No eviction by default; just track. | Full |
| Medium | Shrink more aggressively (target ~70% of budgets). Prefer evicting QueryCache warm tier over Salsa DB rebuild. | Drop closed-file trees. | Drop symbol-search + in-memory index caches first. | Still avoid full rebuild/drop; consider clearing internal memo tables only. | Full |
| High | `flush_to_disk()` best-effort, then shrink to ~50%. Salsa memo eviction allowed if still over target. | Drop closed-file trees; consider keeping open-file trees if possible. | Persist + drop index overlays; keep only smallest useful subset. | If memory is still not recoverable, allow dropping/reloading indexes (last resort). | Reduced |
| Critical | `flush_to_disk()` then clear everything evictable. Must not panic. | Allow dropping even open-file trees if needed (but never text). | Clear all in-memory indexes (keep disk view only). | Drop large caches to keep process alive (expect degraded answers). | Paused |

Notes:

- “Keep only smallest useful subset” for indexes often means: keep *symbol locations* but drop
  references/inheritance/annotations first (if partial retention is supported).
- Salsa memo eviction is currently “all-or-nothing” (DB rebuild). Treat it as expensive / last
  resort unless/until Salsa exposes a stable sweep API.

---

## 4) What must be persisted before eviction to preserve warm-start UX?

Before destructive eviction (especially `High`/`Critical`), we want to persist what we can so that:

- A follow-up query becomes a **cache hit** (disk) instead of a slow recompute.
- After a restart, Nova can warm-start quickly.

### Persistable artifacts (today)

1. **QueryCache warm tier → disk**
   - `QueryCache::flush_to_disk()` already exists.
2. **AST/HIR “file artifacts” → disk**
   - `nova-db` persists `FileAstArtifacts` (parse + token item tree) when computing `item_tree`
     (`crates/nova-db/src/salsa/semantic.rs`).
3. **Project indexes → disk**
   - `nova-index` already has sharded index persistence used by `nova-workspace` CLI.
   - The workspace engine (`crates/nova-workspace/src/engine.rs`) keeps an owned in-memory
     `ProjectIndexes` that is currently not tied into persistence during eviction.
4. **Classpath entry stubs → disk**
   - `nova-classpath` uses best-effort per-entry persistence for class directories, and also
     consumes `nova-deps-cache` bundles for JARs when available.
5. **JDK indexing caches → disk**
   - `nova-jdk` has a `persist` module; eviction should prefer clearing in-memory maps after
     ensuring on-disk caches exist.

### Missing flush hooks

- Workspace engine needs a `flush_to_disk()` hook for its in-memory `ProjectIndexes` (persist if
  dirty before dropping).
- If we add evictors for classpath/JDK indexes, they should expose a best-effort `flush_to_disk()`
  (or “ensure persisted”) before clearing in-memory caches.

---

## 5) Ensuring open docs + frequently used results stay available (or cheaply re-warm)

### Hard pinned: open document text

Open document text (editor buffers) is correctness-critical. Eviction must never remove it.

Action item: ensure **all** sources of in-memory overlay text are tracked (not just “open docs”).

### Soft pinned: open document derived artifacts

For derived artifacts, we prefer to keep them for open docs, but can drop them under severe
pressure because they are cheaply recomputed from pinned text:

- Syntax trees for open docs: keep if possible; drop under `Critical` if needed.
- Per-open-doc indexes: keep if we implement file-granular index retention.

### “Frequently used” ≈ small MRU windows

For large-scale caches (indexes, query results), an effective strategy is to maintain a small MRU:

- Keep results for:
  - open documents
  - recently navigated files (go-to-def targets)
  - recently completed files
- Evict the rest first.

This requires file-level attribution for caches that currently only know “total bytes”.

---

## Implementation plan: worker-ready tasks

The tasks below are designed to be independently executable by workers and to converge to a
coherent end-to-end system.

### Track A — Accounting completeness (make pressure actionable)

1. **`nova-index`: add heap size estimation helpers**
   - Add `estimated_bytes()` (best-effort) for:
     - `ProjectIndexes`
     - `SymbolIndex`, `ReferenceIndex`, `InheritanceIndex`, `AnnotationIndex`
   - Use capacities + string capacities; follow the style of
     `SymbolSearchIndex::estimated_bytes()` (`crates/nova-index/src/symbol_search.rs`).
   - Unit tests: “estimate grows with inserted data”; “estimate is non-zero for non-empty”.

2. **`nova-workspace`: track in-memory `ProjectIndexes`**
   - Introduce a `WorkspaceIndexStore` wrapper that:
     - holds the `ProjectIndexes`
     - registers a `MemoryTracker` (category `Indexes`)
     - refreshes bytes after index updates using `estimated_bytes()`
   - Unit tests for accounting: tracker reflects growth and clears after eviction.

3. **Track classpath/JDK index memory**
   - `nova-classpath`: add `estimated_bytes()` for `ClasspathIndex`.
   - `nova-jdk`: add `estimated_bytes()` for `JdkIndex` (separate builtin vs symbol-backed).
   - Wire trackers in the owning layer:
     - workspace engine for `ClasspathIndex`
     - LSP server and/or workspace engine for `JdkIndex`

### Track B — Eviction integration (make pressure reduce RSS without breaking UX)

4. **`nova-workspace`: evictor for in-memory `ProjectIndexes`**
   - Implement `MemoryEvictor`:
     - `flush_to_disk()` persists indexes when possible (project cache dir).
     - `evict()` drops cold parts first.
   - Proposed eviction ladder:
     - Medium: drop `annotations` + `inheritance` first (if we expose partial clears).
     - High: keep only `symbols`.
     - Critical: clear everything (fall back to disk view for workspace queries).
   - Unit tests: each pressure level produces expected retained subsets.

5. **`nova-workspace`: classpath index eviction hook**
   - Add a memory evictor that can drop `ClasspathIndex` (set Salsa input to `None`) at `High`/`Critical`.
   - Ensure queries behave deterministically with `None` classpath (they already support this in tests).
   - Unit tests: under small budgets, classpath index is dropped and queries still return (possibly degraded) results without panicking.

6. **`nova-jdk`: clear in-memory caches under pressure**
   - Add a method (or evictor) that clears large memo tables inside the symbol-backed index, while
     keeping minimal discovery/metadata.
   - Unit tests: repeated lookups after eviction still succeed (cache miss → recompute), no panic.

7. **`nova-db`: refine Salsa memo eviction behavior**
   - Optional but valuable: treat `SalsaMemoEvictor` as expensive and avoid triggering it under
     `Low/Medium` unless absolutely necessary.
   - If we keep the “DB rebuild” approach, add tests to avoid thrash:
     - repeated `enforce()` cycles with steady-state usage should not rebuild infinitely.

### Track C — System integration + regression protection

8. **Ensure `MemoryManager::enforce()` is driven from the right places**
   - LSP: already enforced on document memory refresh; also enforce after heavy operations
     (project reload, indexing completion).
   - Workspace engine: enforce after:
     - indexing batch completion
     - classpath rebuild
     - large file loads
   - Add a small periodic timer (optional) to avoid “never enforced” failure modes.

9. **Use degraded settings to keep UX responsive**
   - Wire `MemoryManager::degraded_settings()` into:
     - background indexing scheduling (pause/reduce)
     - expensive diagnostics (skip under `High/Critical`)
     - completion candidate limits (already exists in `nova-memory`, needs call sites)
   - Unit tests: degraded flags flip when pressure changes.

### Track D — Tests

10. **Unit tests for each evictor**
    - Already done for `QueryCache` and `SalsaMemoEvictor` in `nova-db`.
    - Add unit tests for new index/classpath/jdk evictors.

11. **Stress test / integration test (budget constrained)**
    - Goal: prove “Nova remains responsive” under tight budgets:
      - no panics
      - eviction happens deterministically
      - caches can re-warm (observably) after eviction
    - Candidate home: `crates/nova-workspace/tests/` using a temp workspace with many Java files.
      - Set `MemoryBudget::from_total(<tiny>)`
      - Run indexing + a representative query loop (diagnostics, workspace symbols, go-to-def)
      - Assert:
        - `MemoryManager::report().pressure` does not stay `Critical` indefinitely after eviction
        - key queries still succeed (even if slower / degraded)

---

## Open questions / design risks

1. **Single vs multiple `MemoryManager`s in-process**
   - Today, `nova-lsp` and `nova-workspace` may each create their own managers.
   - Long-term we likely want a single shared manager per process to avoid “double budgeting”.
2. **Salsa memo eviction granularity**
   - Current solution rebuilds the DB to drop memos. This is safe for snapshots but expensive.
   - If Salsa exposes a stable sweep API, we should migrate to partial, file-granular eviction.
3. **mmap-backed archives**
   - Index and dependency caches may be mmap-backed and counted in RSS.
   - We should treat RSS as authoritative and ensure eviction can reduce RSS (by dropping mappings
     / archives) when needed.

