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
| `Indexes` | Cross-file/workspace indexes used for search/navigation; may have disk-backed warm-start formats. | `nova_index::WorkspaceSymbolSearcher`, `nova_index::IndexCache`, workspace-held `ProjectIndexes` (`workspace_project_indexes`) |
| `TypeInfo` | Typechecking/type inference caches and expensive semantic models. Also used for large external type indexes (JDK/classpath) that can be reduced under pressure. | `jdk_index` / `classpath_index` evictors (`nova_db::salsa::InputIndexTracker`) |
| `Other` | Everything else (inputs, overlays, glue). This is also where we track memory that we **cannot** currently evict. | `salsa_inputs` tracker, LSP `vfs_documents` tracker |

### Avoiding double-counting (important)

When a single allocation is shared across layers, only one layer should account for it.

Concrete example: Salsa parse results are shared between memo tables and the open-document syntax tree store.

- Salsa memo tracking explicitly records **0 bytes** for pinned results to avoid double-counting:
  - See `TrackedSalsaMemo::{Parse, ParseJava, ItemTree}` in `crates/nova-db/src/salsa/mod.rs`.
- There are integration tests asserting these invariants:
  - `crates/nova-db/src/salsa/mod.rs` — `open_doc_parse_is_not_double_counted_between_query_cache_and_syntax_trees`
  - `crates/nova-db/src/salsa/mod.rs` — `open_doc_parse_java_is_not_double_counted_between_query_cache_and_syntax_trees`
  - `crates/nova-db/src/salsa/mod.rs` — `open_doc_item_tree_is_not_double_counted_between_query_cache_and_syntax_trees`

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
   - `ServerState::refresh_document_memory()` updates the `vfs_documents` tracker and then calls `memory.enforce()`.
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
  - Approximation via `SalsaMemoFootprint` (in `crates/nova-db/src/salsa/mod.rs`) for selected memo tables, recorded explicitly by query implementations:
    - **File-keyed** memos: `TrackedSalsaMemo` (e.g. `Parse`, `ParseJava`, `ItemTree`, `FileIndexDelta`, plus additional tracked file memos in `crates/nova-db/src/salsa/{syntax,semantic,hir,resolve}.rs`)
    - **Project-keyed** memos: `TrackedSalsaProjectMemo`, notably:
      - `ProjectIndexShards` (`NovaIndexing::project_index_shards` in `crates/nova-db/src/salsa/indexing.rs`)
      - `ProjectIndexes` (`NovaIndexing::project_indexes` in `crates/nova-db/src/salsa/indexing.rs`)
      - `WorkspaceDefMap` (`NovaResolve::workspace_def_map` in `crates/nova-db/src/salsa/resolve.rs`)
      - `ProjectBaseTypeStore` (`NovaTypeck::project_base_type_store` in `crates/nova-db/src/salsa/typeck.rs`)
      - `JpmsCompilationEnv` (`NovaResolve::jpms_compilation_env` in `crates/nova-db/src/salsa/resolve.rs`)
    - **Project+module-keyed** memos: `TrackedSalsaProjectModuleMemo`, notably:
      - `ProjectBaseTypeStoreForModule` (`NovaTypeck::project_base_type_store_for_module` in `crates/nova-db/src/salsa/typeck.rs`)
    - **Body-keyed** memos: `TrackedSalsaBodyMemo` (recorded in `crates/nova-db/src/salsa/{hir,flow,typeck}.rs`)
- `evict(request)`:
  - Best-effort rebuild: clones `SalsaInputs` and rebuilds `RootDatabase` behind a mutex, dropping memoized results.
  - Interned tables (`#[ra_salsa::interned]`) are snapshot+restored **for the subset included in**
    `crates/nova-db/src/salsa/mod.rs:InternedTablesSnapshot`, so those interned ids remain stable
    across memo eviction.
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

#### `workspace_closed_file_texts` (workspace-held closed file texts)

- Code: `crates/nova-workspace/src/engine.rs` (`ClosedFileTextStore`)
- Category: `MemoryCategory::QueryCache`
- Registration: `ClosedFileTextStore::new` (`MemoryManager::register_evictor("workspace_closed_file_texts", …)`)
- Tracked bytes: sum of in-memory `Arc<String>` allocations for *closed* file contents.
- Double-counting: when a file is tracked by this store, its `file_content` bytes are suppressed from `salsa_inputs` via `Database::set_file_text_suppressed(file, true)` to avoid double-counting.
- `evict(request)`:
  - Chooses large closed-file allocations as candidates (never evicts open docs).
  - Replaces the Salsa input `file_content(file)` with a shared empty `Arc<String>` (`empty_file_content()`) to drop the large allocation while keeping inputs well-formed.
  - Marks `file_is_dirty(file)=true` so persistence does not overwrite on-disk caches with placeholder contents.
  - Restores text lazily on demand from the VFS (`ClosedFileTextStore::restore_if_evicted`).
- `flush_to_disk()`:
  - Not implemented (default no-op).

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

#### `nova_syntax::JavaParseStore`

- Code: `crates/nova-syntax/src/java_parse_store.rs`
- Category: `MemoryCategory::SyntaxTrees`
- Tracked bytes: approximate (sum of pinned Java parse source text lengths)
- `evict(request)`:
  - `Low/Medium/High`: drops trees for closed files (keeps open docs warm).
  - `Critical`: clears all stored trees.
- `flush_to_disk()`:
  - Not implemented (default no-op).
- Correctness constraints:
  - Designed to keep open-doc `parse_java` results alive across Salsa memo eviction (DB rebuild).
  - When integrated with Salsa, pinned `parse_java` results should be recorded as `0` bytes in the Salsa memo footprint to avoid double-counting.

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

#### `nova_db::salsa::JavaParseCache`

- Code: `crates/nova-db/src/salsa/java_parse_cache.rs`
- Category: `MemoryCategory::SyntaxTrees`
- Registration: `Database::register_java_parse_cache_evictor` (called by `Database::register_salsa_memo_evictor`)
- Tracked bytes: intentionally **0** to avoid double-counting shared `Arc<JavaParseResult>` allocations (accounted via Salsa memo footprint tracking instead).
- `evict(request)`:
  - Clears the cache on `target_bytes == 0` or `Critical` pressure (best-effort).
- `flush_to_disk()`:
  - Not implemented (default no-op).

### `Indexes` category

#### `workspace_project_indexes` (workspace-held `nova_index::ProjectIndexes`)

- Code: `crates/nova-workspace/src/engine.rs` (`WorkspaceProjectIndexesEvictor`)
- Category: `MemoryCategory::Indexes`
- Registration: `MemoryManager::register_evictor("workspace_project_indexes", …)`
- Tracked bytes: `ProjectIndexes::estimated_bytes()`
- `evict(request)`:
  - Current semantics: if asked to shrink (i.e. `target_bytes < current`) or under `Critical`, drops the in-memory indexes entirely (`ProjectIndexes::default()`).
  - Indexes will be rebuilt lazily by the next indexing/search operation.
- `flush_to_disk()`:
  - Not implemented (default no-op).

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

### `TypeInfo` category

#### Evictors: `jdk_index`, `classpath_index` (Salsa input index tracking)

- Code: `crates/nova-db/src/salsa/mod.rs` (`InputIndexTracker`, `JdkIndexEvictor`, `ClasspathIndexEvictor`)
- Category: `MemoryCategory::TypeInfo`
- Registration:
  - `Database::register_input_index_trackers` (called from `Database::register_salsa_memo_evictor`)
  - `InputIndexTracker::register_evictor` calls `MemoryManager::register_evictor(..., MemoryCategory::TypeInfo, ...)`
- Tracked bytes: best-effort estimated sizes, de-duplicated across projects via pointer identity (see `InputIndexTrackerInner::{by_project,by_ptr}`)
- `evict(request)`:
  - `jdk_index` (`JdkIndexEvictor`):
    - If `before_bytes > target_bytes` and pressure is `Medium+`, clears per-index symbol caches via `nova_jdk::JdkIndex::evict_symbol_caches()` (does **not** drop the index input itself).
    - Best-effort: uses `try_lock`; if locks can’t be acquired, eviction is a no-op.
  - `classpath_index` (`ClasspathIndexEvictor`):
    - Has `eviction_priority = 10` (evict later than “cheaper” `TypeInfo` caches).
    - Only drops classpath indexes under `High/Critical` pressure by setting the Salsa input `classpath_index(project)` to `None` (large UX hit, but safe under degraded mode).
- `flush_to_disk()`:
  - Not implemented (default no-op).

### `Other` category

#### Tracker: `salsa_inputs`

- Code: `crates/nova-db/src/salsa/mod.rs` (`SalsaInputFootprint`)
- Category: `MemoryCategory::Other`
- Tracked bytes: best-effort tracked input sizes, including:
  - file text lengths (current + previous incremental-parse snapshot + last edit replacement bytes), unless suppressed
  - other small inputs (e.g. file rel paths, `all_file_ids`, project configs/files/class ids)
- Double-counting: hosts can suppress tracking for selected file texts via `Database::set_file_text_suppressed` (e.g. when a separate evictable store owns those allocations).
- Eviction: none (trackers only)

#### Tracker: `vfs_documents` (LSP)

- Code: `crates/nova-lsp/src/main.rs` (`documents_memory`)
- Category: `MemoryCategory::Other`
- Tracked bytes: `Vfs::estimated_bytes()` (overlay documents + cached virtual documents)
- Eviction: none (trackers only)

#### Tracker: `vfs_overlay_documents` (workspace VFS overlay)

- Code: `crates/nova-workspace/src/engine.rs` (`overlay_docs_memory_registration`, `sync_overlay_documents_memory`)
- Category: `MemoryCategory::Other`
- Tracked bytes: `Vfs::overlay().estimated_bytes()`
- Eviction: none (trackers only)

---

## Known gaps (tracked vs untracked) and follow-up work

This section is intentionally written as **worker-ready tasks** (clear entrypoints + acceptance criteria).

### Gap: VFS overlay + virtual document store are only partially tracked and not evictable

Symptoms:

- We track the workspace overlay text under `vfs_overlay_documents`, but this does not cover all VFS-related allocations, notably:
  - the `VirtualDocumentStore` used for decompiled sources (`crates/nova-vfs/src/virtual_documents.rs`)
  - VFS internal overhead (path maps, versions, snapshots)
- There is no memory-manager-driven eviction strategy for overlays or virtual documents (beyond any fixed internal LRU/budgeting).

#### Follow-up task 1: Track + evict `VirtualDocumentStore` bytes under memory pressure

- Owner: `nova-vfs` (plus integration in `nova-lsp` / `nova-workspace` as needed)
- Entry points:
  - `crates/nova-vfs/src/virtual_documents.rs` (`VirtualDocumentStore`)
  - `crates/nova-vfs/src/vfs.rs` (where the store is constructed/owned)
- Approach:
  1. Add a best-effort size estimate API (e.g. `estimated_bytes()`) and register a `MemoryTracker` so this store shows up in memory status.
  2. Optionally implement a `MemoryEvictor` that clears the LRU (or lowers its internal byte budget) under `High/Critical`.
- Acceptance criteria:
  - `MemoryManager::report_detailed()` includes a component for virtual documents.
  - Under forced `Critical` pressure, virtual documents are dropped and later regenerated on demand.

### Gap: ensure all disk flushes under pressure persist only “safe” artifacts

Symptoms:

- `MemoryManager` calls `flush_to_disk()` under pressure **before eviction** (by design).
- Any future `flush_to_disk()` implementations that persist derived artifacts must ensure:
  - they never serialize content derived from dirty overlays (or they must include overlay fingerprints in the cache key)
  - failures are treated as cache misses (never correctness)

#### Follow-up task 2: Standardize “safe persistence” guardrails for `flush_to_disk()`

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
