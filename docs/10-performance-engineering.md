# 10 - Performance Engineering

[← Back to Main Document](../AGENTS.md) | [Previous: Framework Support](09-framework-support.md)

## Overview

Performance is not an afterthought—it's a core design requirement. Nova must be fast enough that users never wait, efficient enough to run on laptops, and scalable enough to handle massive codebases.

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
│  • Eviction: frequency-based                                    │
│  • Lookup: O(1) hash table                                      │
│                                                                  │
│  L3: COLD CACHE (memory-mapped)                                 │
│  • Persistent across restarts                                   │
│  • Size: project-dependent                                      │
│  • Lookup: O(log n) B-tree                                      │
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
    /// Hot cache for recently accessed
    hot: LruCache<QueryKey, CachedValue>,
    
    /// Warm cache for current working set
    warm: HashMap<QueryKey, CachedValue>,
    
    /// Memory-mapped persistent cache
    cold: MmapCache,
    
    /// Dependency tracking
    deps: DependencyGraph,
}

impl QueryCache {
    pub fn get(&self, key: &QueryKey) -> Option<&CachedValue> {
        // Check L1
        if let Some(value) = self.hot.get(key) {
            return Some(value);
        }
        
        // Check L2
        if let Some(value) = self.warm.get(key) {
            // Promote to L1
            self.hot.put(key.clone(), value.clone());
            return Some(value);
        }
        
        // Check L3
        if let Some(value) = self.cold.get(key) {
            // Promote to L2 and L1
            self.warm.insert(key.clone(), value.clone());
            self.hot.put(key.clone(), value.clone());
            return Some(value);
        }
        
        None
    }
    
    pub fn invalidate(&mut self, key: &QueryKey) {
        // Remove from all levels
        self.hot.pop(key);
        self.warm.remove(key);
        self.cold.remove(key);
        
        // Invalidate dependents
        for dependent in self.deps.dependents(key) {
            self.invalidate(&dependent);
        }
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

```rust
pub struct MemoryBudget {
    /// Total budget
    total: usize,
    
    /// Allocation per category
    categories: HashMap<Category, usize>,
}

impl MemoryBudget {
    pub fn default_for_system() -> Self {
        let total_ram = system_memory();
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

### Memory Pressure Handling

```rust
impl MemoryManager {
    /// Called when memory usage exceeds threshold
    pub fn handle_memory_pressure(&mut self, pressure: MemoryPressure) {
        match pressure {
            MemoryPressure::Low => {
                // Start proactive eviction
                self.query_cache.evict_cold();
            }
            
            MemoryPressure::Medium => {
                // Aggressive cache eviction
                self.query_cache.evict_to_target(self.budget.total * 70 / 100);
                
                // Release syntax trees for closed files
                self.syntax_trees.release_closed_files();
            }
            
            MemoryPressure::High => {
                // Emergency measures
                self.query_cache.clear_cold();
                self.syntax_trees.release_all_closed();
                
                // Force GC of weak references
                self.force_gc();
                
                // Consider persisting to disk
                self.flush_to_disk();
            }
            
            MemoryPressure::Critical => {
                // Extreme measures
                self.query_cache.clear_all();
                self.syntax_trees.clear_all();
                
                // Signal degraded mode
                self.enter_degraded_mode();
            }
        }
    }
}
```

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
│  │   ├── types.cache          # Type resolution cache           │
│  │   └── signatures.cache     # Method signatures               │
│  ├── ast/                                                       │
│  │   └── *.ast               # Serialized syntax trees          │
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
