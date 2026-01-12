# 20 - Memory Management and Eviction

This document is the **source of truth** for Nova’s memory accounting + eviction integration.
It is intended to answer (with pointers to real code):

1. What memory is tracked (and what is not)?
2. What gets evicted at each pressure level?
3. What must be flushed to disk before eviction?
4. How do we preserve correctness when persisting caches (especially with dirty overlays)?

Primary implementation references:

- `crates/nova-memory/src/manager.rs` (`MemoryManager::enforce`)
- `crates/nova-memory/src/types.rs` (`MemoryCategory`)
- `crates/nova-memory/src/budget.rs` (`MemoryBudget`, overrides)
- `crates/nova-memory/src/eviction.rs` (`MemoryEvictor`, `flush_to_disk`)

---

## Mental model

Nova’s memory system is **cooperative**:

- Components opt-in by registering a `MemoryTracker` (accounting only) or a `MemoryEvictor` (accounting + eviction).
- `MemoryManager::enforce()` is called by higher-level drivers (workspace indexing, LSP requests, etc.) to:
  - compute current pressure
  - optionally `flush_to_disk()` (High/Critical)
  - ask evictors to reduce memory to per-category targets
  - update “degraded” feature toggles

Because enforcement is best-effort, correctness must *never* depend on eviction or disk persistence succeeding.

---

## Memory categories (`MemoryCategory`)

Defined in `crates/nova-memory/src/types.rs`.

These categories exist to keep budgeting and eviction coarse-grained and predictable.
Pick the **closest semantic bucket** for a component; avoid “Other” unless there is no better option.

| Category | Intended contents | Examples in-tree |
|---|---|---|
| `QueryCache` | Recomputable cached query results; memo tables; anything “Salsa-like” where eviction means “drop cached results and recompute later”. | `nova_db::QueryCache`, `nova_db::salsa::SalsaMemoEvictor` |
| `SyntaxTrees` | Parsed/structural per-file artifacts closely tied to source text; caches that should prefer keeping *open* documents warm. | `nova_syntax::SyntaxTreeStore`, `nova_db::salsa::ItemTreeStore` |
| `Indexes` | Cross-file/workspace indexes used for search/navigation; may have disk-backed warm-start formats. | `nova_index::WorkspaceSymbolSearcher`, `nova_index::IndexCache`, `jdk_index`/`classpath_index` trackers |
| `TypeInfo` | Typechecking/type inference caches and expensive semantic models (reserved; not heavily used yet). | (gap: no major registrations today) |
| `Other` | Everything else (inputs, overlays, glue). This is also where we track memory that we **cannot** currently evict. | `salsa_inputs` tracker, LSP `open_documents` tracker |

### Avoiding double-counting (important)

When a single allocation is shared across layers, only one layer should account for it.

Concrete example: Salsa parse results are shared between memo tables and the open-document syntax tree store.

- Salsa memo tracking explicitly records **0 bytes** for pinned parses to avoid double-counting:
  - See `TrackedSalsaMemo::Parse` in `crates/nova-db/src/salsa/mod.rs`.
- There is an integration test asserting this invariant:
  - `crates/nova-db/src/salsa/mod.rs` — `open_doc_parse_is_not_double_counted_between_query_cache_and_syntax_trees`

---

## Budgets: defaults + overrides (config + env)

### Default total budget

`MemoryBudget::default_for_system()` (see `crates/nova-memory/src/budget.rs`) derives a budget from the effective system memory ceiling:

- effective memory = **min** of:
  - host total RAM
  - Linux cgroup limit (if present)
  - process `RLIMIT_AS` (address space) (if set)
- budget = `min(total_ram / 4, 4GiB)` clamped to at least `512MiB`

This is designed to behave well in containers/sandboxes.

### Default per-category split

`MemoryBudget::from_total(total)` splits by percentages (also referenced in `docs/10-performance-engineering.md`):

- `QueryCache`: 40%
- `SyntaxTrees`: 25%
- `Indexes`: 20%
- `TypeInfo`: 10%
- `Other`: remainder (so `sum(categories) == total`)

### Overrides and precedence

Budgets can be overridden in two places:

1. **Config overrides** via `nova-config`:
   - `nova_config::MemoryConfig` → `nova_memory::MemoryBudgetOverrides`
   - See `crates/nova-config/src/lib.rs` (`memory_budget_overrides`)
2. **Environment overrides** via `MemoryBudgetOverrides::from_env()`:
   - See constants in `crates/nova-memory/src/budget.rs`:
     - `NOVA_MEMORY_BUDGET_TOTAL`
     - `NOVA_MEMORY_BUDGET_QUERY_CACHE`
     - `NOVA_MEMORY_BUDGET_SYNTAX_TREES`
     - `NOVA_MEMORY_BUDGET_INDEXES`
     - `NOVA_MEMORY_BUDGET_TYPE_INFO`
     - `NOVA_MEMORY_BUDGET_OTHER`

Current precedence in Nova binaries (workspace + LSP):

```rust
let budget = MemoryBudget::default_for_system()
    .apply_overrides(config_overrides)
    .apply_overrides(MemoryBudgetOverrides::from_env());
```

This means: **env wins over config**, and setting `total` re-derives the default category split before applying per-category overrides.

---

## Pressure, degraded mode, and what `enforce()` actually does

### Pressure computation

Pressure is computed from `usage_total / budget_total` with thresholds (default in `crates/nova-memory/src/pressure.rs`):

- `Medium`: `>= 0.70`
- `High`: `>= 0.85`
- `Critical`: `>= 0.95`

On Linux, process RSS is incorporated as an upper bound:

- effective_total = `max(tracked_total, rss_bytes)` (see `MemoryManager::pressure` in `crates/nova-memory/src/manager.rs`)

This is intentionally conservative: if we are undercounting, RSS will still drive degraded behavior.

### Degraded settings

`MemoryManager` derives “degraded settings” from the pressure level (see `crates/nova-memory/src/degraded.rs`), e.g.:

- `High` / `Critical`: skip expensive diagnostics, cap completions, reduce/pause background indexing.

### Enforcement behavior (ordering matters)

`MemoryManager::enforce()` (see `crates/nova-memory/src/manager.rs`) does:

1. Compute **before** pressure
2. If before pressure is `High` or `Critical`:
   - call `flush_to_disk()` on **all registered evictors**, ignoring errors
3. Compute a per-category target ratio and evict:
   - `Low`: ratio `1.0` (still enforces per-category budgets)
   - `Medium`: ratio `0.70`
   - `High`: ratio `0.50`
   - `Critical`: ratio `0.0`
4. Emit a `MemoryEvent` if pressure/degraded settings changed

There is a dedicated test ensuring **flush happens before eviction** under High/Critical:

- `crates/nova-memory/tests/suite/memory_manager.rs` — `enforce_flushes_to_disk_before_evicting_under_high_and_critical_pressure`

---

## What drives enforcement today (call sites) + recommended cadence

`enforce()` is synchronous and deterministic; it should be called at **controlled cadence points** (not on every keystroke unless heavily debounced).

Current call sites in-tree:

1. Workspace background indexing (`crates/nova-workspace/src/engine.rs`)
   - `WorkspaceEngine::trigger_indexing()` calls `memory.enforce()` before starting indexing to gate background work.
   - While indexing is running, a dedicated thread calls `memory.enforce()` periodically (`ENFORCE_INTERVAL = 250ms`) and cancels indexing on `Critical`.
2. Workspace symbol search (`crates/nova-workspace/src/lib.rs`)
   - `Workspace::workspace_symbols_cancelable()` calls `memory.enforce()` after index build/search work.
3. LSP document memory updates (`crates/nova-lsp/src/main.rs`)
   - `ServerState::refresh_document_memory()` updates the `open_documents` tracker and then calls `memory.enforce()`.
4. LSP memory status request (`crates/nova-lsp/src/main.rs`)
   - `nova_lsp::MEMORY_STATUS_METHOD` forces an `enforce()` pass to return up-to-date pressure and trigger eviction before reporting.

Recommended enforcement cadence for new integrations:

- Call `enforce()`:
  - **before** starting a potentially memory-heavy batch (indexing, scanning, bulk symbol search)
  - on a **periodic timer** while long-running background work is executing (current indexing uses 250ms)
  - after large caches are (re)built, especially if they are optional and can be evicted later
- Avoid calling `enforce()` on every tiny update; prefer debounced/periodic enforcement in hot paths.

---

## Registered components / evictors (current “source of truth”)

This list is meant to be kept accurate as new components integrate.

### `QueryCache` category

#### `nova_db::QueryCache`

- Code: `crates/nova-db/src/query_cache.rs`
- Category: `MemoryCategory::QueryCache`
- Registration: `MemoryManager::register_evictor("query_cache", …)`
- Tracked bytes: `hot.bytes + warm.bytes` (raw `Vec<u8>` payload sizes)
- `evict(request)`:
  - If `target_bytes == 0`: clears hot + warm tiers.
  - Else: shrinks with a small hot tier and larger warm tier (hot ~= 20% of target).
  - Under `High/Critical`, LRU eviction prefers dropping entries (optionally storing them to disk).
- `flush_to_disk()`:
  - If constructed with a disk cache dir, writes warm tier entries to `nova_cache::QueryDiskCache`.
  - Best-effort; errors are ignored by the manager.
- Correctness constraints:
  - Values are stored behind `Arc<Vec<u8>>` so dropping cache references is snapshot-safe.
  - Disk cache directory must be **project-scoped** (`QueryCache::new_with_disk` warns about cross-project key collisions).
  - The disk cache is a performance cache; corruption/version mismatch is treated as miss (never correctness).

#### `nova_db::salsa::SalsaMemoEvictor`

- Code: `crates/nova-db/src/salsa/mod.rs` (type `SalsaMemoEvictor`)
- Category: `MemoryCategory::QueryCache`
- Registration: via `Database::register_salsa_memo_evictor(&MemoryManager)`
  - Also registers additional trackers as a side effect (see below).
- Tracked bytes:
  - Approximation via `SalsaMemoFootprint` for selected file-keyed memo tables (`TrackedSalsaMemo`).
- `evict(request)`:
  - Best-effort rebuild: clones `SalsaInputs` and rebuilds `RootDatabase` behind a mutex, dropping memoized results.
  - Clears the memo footprint tracker; memos will be re-recorded as queries re-run.
  - Outstanding Salsa snapshots remain valid (they own storage snapshots).
- `flush_to_disk()` (called by manager under High/Critical):
  - Best-effort persistence of project index shards by calling `Database::persist_project_indexes(project)` for “known projects”.
  - Skips entirely when persistence mode doesn’t allow writes or no cache dir exists.
  - Uses `catch_unwind` internally (must never panic during memory pressure handling).
- Correctness constraints:
  - **Must not persist indexes when any file is dirty** (in-memory overlays not reflected on disk).
    - Enforced by `Database::persist_project_indexes`: it returns early if `project_files(project)` contains any `file_is_dirty(file)`.
  - `flush_to_disk()` must remain best-effort and non-panicking; eviction must not be blocked on I/O.

### `SyntaxTrees` category

#### `nova_syntax::SyntaxTreeStore`

- Code: `crates/nova-syntax/src/tree_store.rs`
- Category: `MemoryCategory::SyntaxTrees`
- Tracked bytes: approximate (sum of `parse.root.text_len`)
- `evict(request)`:
  - `Low/Medium/High`: drops trees for closed files (keeps open docs warm).
  - `Critical`: clears all stored trees.
- `flush_to_disk()`:
  - Not implemented (default no-op).
- Correctness constraints:
  - `get_if_current` uses `Arc::ptr_eq` against the current text snapshot to prevent returning stale parses after edits.
  - When paired with Salsa, pinned parses must not be double-counted (see above).

#### `nova_db::salsa::ItemTreeStore`

- Code: `crates/nova-db/src/salsa/item_tree_store.rs`
- Category: `MemoryCategory::SyntaxTrees`
- Tracked bytes: approximate (sum of source text lengths for cached open-doc entries)
- `evict(request)`:
  - `Low/Medium/High`: drops entries for closed docs (retains open docs).
  - `Critical`: clears all.
- `flush_to_disk()`:
  - Not implemented (default no-op).
- Correctness constraints:
  - Designed to keep open-doc `item_tree` results alive across Salsa memo eviction (DB rebuild).
  - Must only reuse entries when the `Arc<String>` text matches by pointer identity.

### `Indexes` category

#### `nova_index::WorkspaceSymbolSearcher`

- Code: `crates/nova-index/src/symbol_search.rs`
- Category: `MemoryCategory::Indexes`
- Tracked bytes: `SymbolSearchIndex::estimated_bytes()`
- `evict(request)`:
  - Drops the cached `SymbolSearchIndex` when the target requires reduction (or `Critical`).
  - Next request will rebuild lazily.
- `flush_to_disk()`:
  - Not implemented (default no-op).
- Correctness/UX constraints:
  - If evicted, `search_with_stats_cached` returns an empty set until rebuilt; callers should tolerate this.

#### `nova_index::IndexCache`

- Code: `crates/nova-index/src/memory_cache.rs`
- Category: `MemoryCategory::Indexes`
- Tracked bytes: sum of cached `Arc<Vec<u8>>` payload sizes
- `evict(request)`:
  - LRU evicts to `target_bytes`, clears on `target_bytes == 0` or `Critical`.
- `flush_to_disk()`:
  - Not implemented (default no-op).

#### Trackers: `jdk_index`, `classpath_index`

- Code: `crates/nova-db/src/salsa/mod.rs` (`InputIndexTracker`)
- Category: `MemoryCategory::Indexes`
- Tracked bytes: best-effort estimated sizes, de-duplicated across projects via pointer identity
- Eviction: none (trackers only)

### `Other` category

#### Tracker: `salsa_inputs`

- Code: `crates/nova-db/src/salsa/mod.rs` (`SalsaInputFootprint`)
- Category: `MemoryCategory::Other`
- Tracked bytes: sum of file content lengths (plus other small tracked inputs)
- Eviction: none (trackers only)

#### Tracker: `open_documents` (LSP)

- Code: `crates/nova-lsp/src/main.rs` (`documents_memory`)
- Category: `MemoryCategory::Other`
- Tracked bytes: sum of open document text lengths (from LSP analysis state)
- Eviction: none (trackers only)

---

## Known gaps (tracked vs untracked) and follow-up work

This section is intentionally written as **worker-ready tasks** (clear entrypoints + acceptance criteria).

### Gap: project/workspace indexes are not a first-class memory participant

Symptoms:

- `nova_index::ProjectIndexes` can be large (`estimated_bytes()` exists), but the live in-memory index held by the workspace is not registered as a `MemoryEvictor`.
- Some indexing artifacts are computed via Salsa (`project_index_shards`, `file_index_delta`) but are not included in the `TrackedSalsaMemo` footprint approximation.

#### Follow-up task 1: Track + evict workspace-held `ProjectIndexes`

- Owner: `nova-workspace` / `nova-index`
- Entry points:
  - `crates/nova-workspace/src/engine.rs` (field `indexes: Arc<Mutex<ProjectIndexes>>`)
  - `crates/nova-index/src/indexes.rs` (`ProjectIndexes::estimated_bytes`)
- Approach:
  1. Introduce a `ProjectIndexesEvictor` that:
     - registers under `MemoryCategory::Indexes`
     - updates its tracker from `ProjectIndexes::estimated_bytes()`
     - under `High` shrinks/clears optional caches; under `Critical` clears aggressively
  2. Ensure reads remain snapshot-safe (consider storing indexes behind `Arc` and swapping atomically rather than mutating in-place).
- Acceptance criteria:
  - Memory status (`nova_lsp::MEMORY_STATUS_METHOD`) reports the new component.
  - Under forced pressure, indexes are dropped and later rebuilt on demand.
  - Add/extend tests in `nova-workspace` verifying eviction doesn’t panic and doesn’t corrupt results (e.g. symbol search still works after rebuild).

#### Follow-up task 2: Track Salsa-derived indexing memo sizes (beyond parse/item_tree)

- Owner: `nova-db`
- Entry points:
  - `crates/nova-db/src/salsa/mod.rs` (`TrackedSalsaMemo`, `SalsaMemoFootprint`)
  - indexing query group in `crates/nova-db/src/salsa/indexing.rs` (for query names)
- Approach:
  1. Expand `TrackedSalsaMemo` (or introduce a new tracker) to include index-heavy memo tables, e.g. `file_index_delta` / `project_index_shards`.
  2. Ensure accounting remains best-effort and avoids double-counting with any workspace-level caches.
- Acceptance criteria:
  - `MemoryManager::report_detailed()` shows meaningful query_cache usage for indexing memos.
  - Under eviction, large memo tables drop and are recomputed correctly.

### Gap: VFS overlays / virtual document store are only partially tracked and not evictable

Symptoms:

- LSP tracks open document text length as `open_documents`, but this does not cover:
  - VFS internal storage overhead (path maps, versions, snapshots)
  - any additional overlay stores used by refactoring / analysis
- There is no memory-manager-driven eviction strategy for overlays (e.g. dropping closed doc history, compressing snapshots).

#### Follow-up task 3: Make VFS overlays a bounded cache + memory participant

- Owner: `nova-vfs` / `nova-lsp` / `nova-workspace`
- Entry points:
  - `crates/nova-vfs` overlay storage types
  - `crates/nova-lsp/src/main.rs` (`refresh_document_memory`)
- Approach:
  1. Decide policy:
     - bounded internal cache (LRU by closed docs / historical versions), **and/or**
     - implement a `MemoryEvictor` to allow dropping non-essential overlay data under pressure
  2. Ensure invariants:
     - open docs are never dropped
     - `file_is_dirty` remains accurate
     - eviction never changes semantic results (only performance)
- Acceptance criteria:
  - Measurable reduction in RSS when many documents are opened/closed repeatedly.
  - A regression test demonstrating closed-doc overlay data is evicted under `Critical` pressure.

### Gap: ensure all disk flushes under pressure persist only “safe” artifacts

Symptoms:

- `MemoryManager` calls `flush_to_disk()` under pressure **before eviction** (by design).
- Any future `flush_to_disk()` implementations that persist derived artifacts must ensure:
  - they never serialize content derived from dirty overlays (or they must include overlay fingerprints in the cache key)
  - failures are treated as cache misses (never correctness)

#### Follow-up task 4: Standardize “safe persistence” guardrails for `flush_to_disk()`

- Owner: all crates implementing `MemoryEvictor` + persistence (`nova-db`, `nova-index`, etc.)
- Entry points:
  - `crates/nova-memory/src/eviction.rs` (trait docs)
  - existing implementation: `SalsaMemoEvictor::flush_to_disk` and `Database::persist_project_indexes`
- Approach:
  1. Document and enforce a standard pattern:
     - If any `file_is_dirty(file)` in the scope → skip persistence (no-op).
     - Only persist artifacts with stable keys (project-scoped + versioned + input fingerprints/metadata).
  2. Add targeted tests:
     - a dirty overlay prevents index persistence (new test near `persist_project_indexes`).
- Acceptance criteria:
  - Disk caches never contain artifacts derived from dirty overlays.
  - Warm-start correctness is preserved across restarts.

