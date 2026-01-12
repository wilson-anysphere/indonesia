# 10 - Performance Engineering

[← Back to Main Document](../AGENTS.md) | [Previous: Framework Support](09-framework-support.md)

## Overview

Performance is not an afterthought—it's a core design requirement. Nova must be fast enough that users never wait, efficient enough to run on laptops, and scalable enough to handle massive codebases.

**Implementation note:** Persistence and cache format decisions are tracked in [ADR 0005](adr/0005-persistence-formats.md), and concurrency/runtime choices are tracked in [ADR 0004](adr/0004-concurrency-model.md). The material in this document is a mix of concrete targets and design sketches.

---

## Performance Targets

```
┌─────────────────────────────────────────────────────────────────┐
│                    PERFORMANCE TARGETS                           │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  STARTUP                                                        │
│  • Cold start to useful: < 2 seconds                            │
│  • Warm start (cached indexes): < 500ms                         │
│  • First completion after open: < 1 second                      │
│                                                                  │
│  INTERACTIVE LATENCY                                             │
│  • Keystroke to diagnostics update: < 100ms                     │
│  • Completion trigger to results: < 50ms                        │
│  • Hover info: < 50ms                                           │
│  • Go to definition: < 50ms                                     │
│  • Find references (100 refs): < 200ms                          │
│                                                                  │
│  BATCH OPERATIONS                                                │
│  • Full file type-check: < 500ms                                │
│  • Project-wide diagnostics: < 30 seconds (parallelized)        │
│  • Rename across 1000 files: < 5 seconds                        │
│                                                                  │
│  MEMORY                                                          │
│  • Baseline (no project): < 50MB                                │
│  • Medium project (100K LOC): < 500MB                           │
│  • Large project (1M LOC): < 2GB                                │
│  • Peak during indexing: < 3GB                                  │
│                                                                  │
│  SCALING                                                         │
│  • Linear or better with project size                           │
│  • No operations slower than O(n log n)                         │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

---

## Indexing System

### Index Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                    INDEX ARCHITECTURE                            │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │                  IN-MEMORY LAYER                         │    │
│  │  • Hot data: currently open files, recent queries       │    │
│  │  • LRU eviction under memory pressure                   │    │
│  │  • Fast hash table lookups                              │    │
│  └─────────────────────────────────────────────────────────┘    │
│                              │                                   │
│                              ▼                                   │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │               MEMORY-MAPPED LAYER                        │    │
│  │  • Persistent indexes (survives restart)                │    │
│  │  • OS manages paging                                    │    │
│  │  • Fast random access                                   │    │
│  │  • Shared across processes                              │    │
│  └─────────────────────────────────────────────────────────┘    │
│                              │                                   │
│                              ▼                                   │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │                   DISK LAYER                             │    │
│  │  • Full index files                                     │    │
│  │  • Compressed storage                                   │    │
│  │  • Background compaction                                │    │
│  └─────────────────────────────────────────────────────────┘    │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Index Types

```rust
/// Symbol index: name → locations
pub struct SymbolIndex {
    /// B-tree for prefix search
    symbols: BTreeMap<String, Vec<SymbolLocation>>,
    
    /// Trigram index for fuzzy matching
    trigrams: TrigramIndex,
}

/// Reference index: symbol → usages
pub struct ReferenceIndex {
    /// Dense storage for common symbols
    references: HashMap<SymbolId, Vec<ReferenceLocation>>,
}

/// Inheritance index: type → subtypes
pub struct InheritanceIndex {
    subtypes: HashMap<TypeId, Vec<TypeId>>,
    supertypes: HashMap<TypeId, Vec<TypeId>>,
}

/// File index: path → file metadata
pub struct FileIndex {
    by_path: HashMap<PathBuf, FileId>,
    by_id: HashMap<FileId, FileMetadata>,
    content_hashes: HashMap<FileId, ContentHash>,
}
```

### Incremental Index Updates

```rust
impl IndexManager {
    /// Update indexes after file change
    pub fn update_file(&mut self, file: FileId, change: FileChange) {
        match change {
            FileChange::Created => {
                self.index_file(file);
            }
            FileChange::Modified => {
                // Remove old entries
                self.remove_file_from_indexes(file);
                // Add new entries
                self.index_file(file);
            }
            FileChange::Deleted => {
                self.remove_file_from_indexes(file);
            }
        }
    }
    
    /// Optimized: only update changed portions
    pub fn update_file_incremental(&mut self, file: FileId, edit: &TextEdit) {
        // Get old and new symbols
        let old_symbols = self.file_symbols.get(&file).cloned().unwrap_or_default();
        let new_symbols = self.compute_symbols(file);
        
        // Diff and apply changes
        let (added, removed) = diff_symbols(&old_symbols, &new_symbols);
        
        for symbol in removed {
            self.symbol_index.remove(file, &symbol);
        }
        
        for symbol in added {
            self.symbol_index.insert(file, &symbol);
        }
    }
}
```

---

## Caching Strategy

### Query Cache

Nova currently uses a **two-tier in-memory query cache** (`nova-db::QueryCache`) with
best-effort on-disk persistence for warm starts:
  - Hot tier: LRU
  - Warm tier: clock/second-chance
  - Optional disk tier: `nova-cache::QueryDiskCache` (versioned, best-effort)

For *semantic* query-result persistence keyed by query name + arguments + input
fingerprints, Nova uses `nova-cache::DerivedArtifactCache` (see
`nova-db::PersistentQueryCache`).

```
┌─────────────────────────────────────────────────────────────────┐
│                    QUERY CACHE STRATEGY                          │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  CACHE LEVELS                                                   │
│                                                                  │
│  L1: HOT CACHE (in-memory, small)                               │
│  • Most recently accessed queries                               │
│  • Size: ~1000 entries                                          │
│  • Eviction: LRU                                                │
│  • Lookup: O(1) hash table                                      │
│                                                                  │
│  L2: WARM CACHE (in-memory, larger)                             │
│  • Queries for open files and dependencies                      │
│  • Size: ~100K entries                                          │
│  • Eviction: clock / second-chance                              │
│  • Lookup: O(1) hash table                                      │
│                                                                  │
│  L3: COLD CACHE (best-effort on disk)                           │
│  • Persistent across restarts (optional)                        │
│  • Version-gated: schema + Nova version                         │
│  • Corruption/mismatch ⇒ cache miss (never correctness)         │
│                                                                  │
│  INVALIDATION                                                   │
│  • Fine-grained: only affected queries                          │
│  • Tracked automatically by query system                        │
│  • Cascade through dependency graph                             │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Cache Implementation

```rust
pub struct QueryCache {
    /// Hot cache for recently accessed entries.
    hot: LruTier,

    /// Warm cache for the current working set.
    warm: ClockTier,

    /// Optional best-effort disk cache for warm starts.
    disk: Option<QueryDiskCache>,
}

impl QueryCache {
    pub fn get(&self, key: &str) -> Option<Arc<Vec<u8>>> {
        // Check hot tier (L1).
        if let Some(value) = self.hot.get(key) {
            return Some(value);
        }

        // Check warm tier (L2) and promote.
        if let Some(value) = self.warm.get(key) {
            self.hot.insert(key.to_string(), value.clone());
            return Some(value);
        }

        // Optional best-effort disk tier (L3) and promote.
        if let Some(disk) = &self.disk {
            if let Ok(Some(bytes)) = disk.load(key) {
                let value = Arc::new(bytes);
                self.warm.insert(key.to_string(), value.clone());
                self.hot.insert(key.to_string(), value.clone());
                return Some(value);
            }
        }

        None
    }

    pub fn insert(&self, key: String, value: Arc<Vec<u8>>) {
        self.hot.insert(key, value);
    }
}
```

---

## Concurrency

### Thread Pool Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                    THREAD ARCHITECTURE                           │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  MAIN THREAD                                                    │
│  • LSP message dispatch                                         │
│  • Low-latency operations                                       │
│  • Write coordination                                           │
│                                                                  │
│  COMPUTE POOL (N threads, N = cores - 1)                        │
│  • Query execution                                              │
│  • Type checking                                                │
│  • Analysis tasks                                               │
│  • Work-stealing scheduler                                      │
│                                                                  │
│  BACKGROUND POOL (2-4 threads)                                  │
│  • Index building                                               │
│  • Cache warming                                                │
│  • Garbage collection                                           │
│                                                                  │
│  IO POOL (async)                                                │
│  • File system operations                                       │
│  • Network requests                                             │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Parallel Query Execution

```rust
/// Execute independent queries in parallel
pub fn parallel_diagnostics(
    db: &dyn Database,
    files: &[FileId],
) -> Vec<(FileId, Vec<Diagnostic>)> {
    files.par_iter()
        .map(|&file| {
            let snapshot = db.snapshot();
            (file, snapshot.diagnostics(file))
        })
        .collect()
}

/// Cancellable background task
pub async fn index_project(
    db: &dyn Database,
    cancel_token: CancellationToken,
) -> Result<(), Cancelled> {
    let files = db.project_files();
    
    for chunk in files.chunks(100) {
        // Check for cancellation
        if cancel_token.is_cancelled() {
            return Err(Cancelled);
        }
        
        // Index chunk in parallel
        chunk.par_iter().for_each(|&file| {
            let snapshot = db.snapshot();
            snapshot.index_file(file);
        });
        
        // Yield to allow other tasks
        tokio::task::yield_now().await;
    }
    
    Ok(())
}
```

---

## Memory Management

### Memory Budget

Nova’s default memory budget is derived from the *effective* memory available to the
process, not just host RAM. In environments with hard caps (containers, agent
wrappers), we budget against the smallest applicable ceiling:

- Linux cgroup memory limit (cgroup v2 `memory.max`, cgroup v1 `memory.limit_in_bytes`)
- `RLIMIT_AS` (process address space limit) when set
- Host total RAM

This ensures eviction and degraded mode trigger *before* the process hits an OS-enforced limit.

```rust
pub struct MemoryBudget {
    /// Total budget
    total: usize,
    
    /// Allocation per category
    categories: HashMap<Category, usize>,
}

impl MemoryBudget {
    pub fn default_for_system() -> Self {
        let total_ram = system_memory_limit(); // min(host RAM, cgroup limit, RLIMIT_AS)
        let budget = (total_ram / 4).min(4 * GB).max(512 * MB);
        
        Self {
            total: budget,
            categories: hashmap! {
                Category::QueryCache => budget * 40 / 100,
                Category::SyntaxTrees => budget * 25 / 100,
                Category::Indexes => budget * 20 / 100,
                Category::TypeInfo => budget * 10 / 100,
                Category::Other => budget * 5 / 100,
            },
        }
    }
}
```

#### Tuning budgets (env + config)

Nova’s in-process caches are governed by `nova_memory::MemoryBudget`. Operators and CI can tune this
without code changes:

- **Defaults**: derived from total RAM (or cgroup limit) as described above.
- **Config**: `[memory]` table in `nova.toml` (see below).
- **Environment**: `NOVA_MEMORY_BUDGET_*` variables.

Precedence (highest wins): **env > config > defaults**.

##### Environment variables

All values accept either:
- raw bytes (e.g. `1073741824`)
- a human-friendly suffix (binary multiples): `K`, `M`, `G`, `T` (e.g. `512M`, `1G`)

Supported variables:
- `NOVA_MEMORY_BUDGET_TOTAL`
- `NOVA_MEMORY_BUDGET_QUERY_CACHE`
- `NOVA_MEMORY_BUDGET_SYNTAX_TREES`
- `NOVA_MEMORY_BUDGET_INDEXES`
- `NOVA_MEMORY_BUDGET_TYPE_INFO`
- `NOVA_MEMORY_BUDGET_OTHER`

Example:

```bash
export NOVA_MEMORY_BUDGET_TOTAL=1G
export NOVA_MEMORY_BUDGET_QUERY_CACHE=512M
```

##### Config (`nova.toml`)

The workspace config supports an optional `[memory]` table:

```toml
[memory]
# Either integer bytes...
total_bytes = 1073741824

# ...or human sizes as strings.
query_cache_bytes = "512M"
syntax_trees_bytes = "256M"
indexes_bytes = "128M"
type_info_bytes = "64M"
other_bytes = "128M"
```

To confirm the effective budget at runtime, call the LSP endpoint `nova/memoryStatus` and inspect
`report.budget`.

### Memory Pressure Handling

```rust
/// The real implementation lives in `nova-memory` and is best-effort:
/// it computes current pressure and asks registered components to evict.
fn tick(manager: &MemoryManager) {
    // Under high/critical pressure, `enforce()` first calls `flush_to_disk()` on
    // evictors (best-effort), then applies proportional eviction targets.
    let report = manager.enforce();

    // Callers can use `report.pressure` / `report.degraded` to gate expensive work.
    if report.degraded.skip_expensive_diagnostics {
        // enter degraded mode
    }
}
```

### Salsa memo eviction (current limitation)

Nova’s Salsa database (`ra_ap_salsa` / `ra_salsa`) memoizes many query results. Under memory
pressure we would ideally evict *per-file* memoized values (e.g. drop memos for cold files while
keeping open/recent files warm).

As of `ra_salsa`/`ra_ap_salsa` **0.0.269**, Nova does **not** have access to a production-safe
public API to drop memoized values for a specific query key. The implementation in
`crates/nova-db/src/salsa/mod.rs::SalsaMemoEvictor` therefore falls back to rebuilding the whole
`RootDatabase` from inputs to clear memo tables (snapshots remain valid).

---

## Persistence

### Persistent State

```
┌─────────────────────────────────────────────────────────────────┐
│                    PERSISTENT STATE                              │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  PROJECT CACHE DIRECTORY                                        │
│  ~/.nova/cache/<project-hash>/                                  │
│  ├── indexes/                                                   │
│  │   ├── symbols.idx          # Symbol index                    │
│  │   ├── references.idx       # Reference index                 │
│  │   ├── inheritance.idx      # Type hierarchy                  │
│  │   └── annotations.idx      # Annotation index                │
│  ├── queries/                                                   │
│  │   ├── <query-name>/                                         │
│  │   │   ├── index.json        # GC metadata (best-effort)       │
│  │   │   └── <fingerprint>.bin # DerivedArtifactCache entries    │
│  │   └── query_cache/         # QueryDiskCache (optional)        │
│  │       └── <fingerprint>.bin # QueryCache disk spill           │
│  ├── ast/                                                       │
│  │   ├── metadata.bin         # AST cache metadata (versioned)   │
│  │   └── <file-key>.ast       # Serialized syntax trees          │
│  └── metadata.json           # Cache metadata and versions      │
│                                                                  │
│  BENEFITS                                                       │
│  • Near-instant startup for known projects                      │
│  • Survives editor restart                                      │
│  • Can be pre-built (CI/CD integration)                         │
│  • Shareable across team members                                │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Cache packaging and team-shared indexes

To accelerate warm starts (especially on CI and for new developer machines), Nova can package a
project’s persistent cache directory into a single archive and install it elsewhere.

**Package contents (tar.zst):**

- `metadata.json` (Nova version + schema version + project fingerprint + per-file fingerprints)
- `indexes/`
- `queries/` (if present)
- `ast/`
- `checksums.json` (per-file SHA-256 manifest for corruption detection)

**CLI (prototype):**

```bash
# Pack cache → single archive
nova cache pack <project-root> --out nova-cache.tar.zst

# Install archive into local ~/.nova/cache/<project-hash>/
nova cache install <project-root> nova-cache.tar.zst

# Fetch + install (HTTP/file; S3 behind feature flag)
nova cache fetch <project-root> https://example.com/nova-cache.tar.zst
```

**Compatibility policy:**

- Reject if Nova version or schema version mismatch.
- If the project fingerprint differs, install `indexes/` only (so the rest can be rebuilt locally).

**GitHub Actions example:**

```yaml
- name: Build Nova cache package
  run: |
    cargo run --locked -p nova-cli -- cache pack . --out nova-cache.tar.zst

- name: Upload Nova cache package
  uses: actions/upload-artifact@v4
  with:
    name: nova-cache
    path: nova-cache.tar.zst
```

### Shared dependency indexes (global)

Project caches alone still waste work: Maven Central/JDK dependencies are shared across many workspaces and re-indexing them per project is unnecessary. Nova maintains a **global dependency index store** keyed by the dependency artifact's **content hash**:

```
~/.nova/cache/deps/<sha256>/classpath.idx
```

Each bundle contains:
- Class stubs (methods/fields/descriptors/signatures/annotations)
- Package listing and package-prefix index
- Optional trigram index for fuzzy class name lookup

Bundles are written atomically and guarded by a lockfile to avoid corruption when multiple Nova processes index the same JAR concurrently.

CLI helpers:
- `nova deps index <jar>` (prebuild a bundle)
- `nova deps pack <out.tar.gz>` / `nova deps install <archive.tar.gz>` (team/CI sharing)

### Cache Versioning

```rust
/// Cache version management
pub struct CacheMetadata {
    /// Nova version that created cache
    nova_version: Version,
    
    /// Schema version for cache format
    schema_version: u32,
    
    /// Project fingerprint (source hash)
    project_fingerprint: Hash,
    
    /// Last update timestamp
    last_updated: SystemTime,
    
    /// Per-file fingerprints
    file_fingerprints: HashMap<PathBuf, Hash>,
}

impl CacheManager {
    pub fn load_cache(&self, project: &Project) -> Result<Cache, CacheError> {
        let metadata = self.read_metadata(project)?;
        
        // Check compatibility
        if !self.is_compatible(&metadata) {
            return Err(CacheError::IncompatibleVersion);
        }
        
        // Check freshness
        let current_fingerprint = project.fingerprint();
        if metadata.project_fingerprint != current_fingerprint {
            // Partial invalidation
            return self.load_with_invalidation(project, &metadata);
        }
        
        self.load_full(project)
    }
    
    fn load_with_invalidation(
        &self, 
        project: &Project, 
        old_metadata: &CacheMetadata,
    ) -> Result<Cache, CacheError> {
        let mut cache = self.load_full(project)?;
        
        // Find changed files
        for (path, new_hash) in project.file_hashes() {
            if old_metadata.file_fingerprints.get(&path) != Some(&new_hash) {
                cache.invalidate_file(&path);
            }
        }
        
        // Find deleted files
        for path in old_metadata.file_fingerprints.keys() {
            if !project.file_exists(path) {
                cache.invalidate_file(path);
            }
        }
        
        Ok(cache)
    }
}
```

---

## Profiling and Optimization

### Built-in Profiling

```rust
/// Performance tracing
pub struct Profiler {
    spans: Vec<Span>,
    enabled: bool,
}

impl Profiler {
    #[inline]
    pub fn span(&self, name: &'static str) -> SpanGuard {
        if self.enabled {
            SpanGuard::new(name, &self.spans)
        } else {
            SpanGuard::noop()
        }
    }
}

// Usage
fn type_check_file(db: &dyn Database, file: FileId) {
    let _span = db.profiler().span("type_check_file");
    
    // ... type checking logic
}
```

### Performance Diagnostics

```rust
/// Detect performance issues
pub fn diagnose_performance(db: &dyn Database) -> PerformanceReport {
    let mut issues = Vec::new();
    
    // Check cache hit rates
    let hit_rate = db.query_cache().hit_rate();
    if hit_rate < 0.8 {
        issues.push(PerformanceIssue::LowCacheHitRate { rate: hit_rate });
    }
    
    // Check for slow queries
    for (query, duration) in db.slow_queries() {
        if duration > Duration::from_millis(100) {
            issues.push(PerformanceIssue::SlowQuery { 
                query: query.name(),
                duration,
            });
        }
    }
    
    // Check memory usage
    let memory = db.memory_usage();
    if memory > db.memory_budget().total {
        issues.push(PerformanceIssue::MemoryOverBudget { 
            usage: memory,
            budget: db.memory_budget().total,
        });
    }
    
    PerformanceReport { issues }
}
```

---

## Benchmarking

For the **operational** performance regression guard (what CI runs, thresholds, how to compare runs),
see:

- [`perf/README.md`](../perf/README.md)
- [`14-testing-infrastructure.md`](14-testing-infrastructure.md) (performance regression tests section)

```rust
/// Standard benchmarks for Nova
#[cfg(test)]
mod benchmarks {
    use criterion::{black_box, criterion_group, Criterion};
    
    fn bench_completion(c: &mut Criterion) {
        let db = setup_test_db();
        let file = db.open_file("src/Main.java");
        
        c.bench_function("completion_50_items", |b| {
            b.iter(|| {
                let completions = db.completions_at(file, Position::new(10, 5));
                black_box(completions)
            })
        });
    }
    
    fn bench_find_references(c: &mut Criterion) {
        let db = setup_large_project_db(); // 100K LOC
        let symbol = db.find_symbol("CommonClass.commonMethod");
        
        c.bench_function("find_references_100", |b| {
            b.iter(|| {
                let refs = db.find_references(symbol, false);
                black_box(refs)
            })
        });
    }
    
    fn bench_type_check(c: &mut Criterion) {
        let db = setup_test_db();
        let file = db.open_file("src/ComplexClass.java"); // 1000 lines
        
        c.bench_function("type_check_1k_lines", |b| {
            b.iter(|| {
                let diags = db.diagnostics(file);
                black_box(diags)
            })
        });
    }
    
    criterion_group!(benches, bench_completion, bench_find_references, bench_type_check);
}
```

---

## Next Steps

1. → [Editor Integration](11-editor-integration.md): LSP implementation
2. → [Debugging Integration](12-debugging-integration.md): DAP and debugging

---

[← Previous: Framework Support](09-framework-support.md) | [Next: Editor Integration →](11-editor-integration.md)
