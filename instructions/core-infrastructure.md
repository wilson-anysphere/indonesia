# Core Infrastructure Workstream

> **⚠️ MANDATORY: Read and follow [AGENTS.md](../AGENTS.md) completely before proceeding.**
> **All rules in AGENTS.md apply at all times. This file adds workstream-specific guidance.**

---

## Scope

This workstream owns the foundational infrastructure that all other components build upon:

| Crate | Purpose |
|-------|---------|
| `nova-core` | Core types, FileId, spans, common utilities |
| `nova-db` | Salsa-based query database, input/derived queries |
| `nova-vfs` | Virtual file system, file watching, overlay documents |
| `nova-cache` | Caching infrastructure, persistence |
| `nova-memory` | Memory budgeting, cache eviction policies |
| `nova-storage` | Persistent storage abstractions |
| `nova-scheduler` | Task scheduling, background work |
| `nova-workspace` | Workspace management, project discovery |
| `nova-config` | Configuration loading and management |
| `nova-ids` | Interned string IDs, symbol IDs |

---

## Key Documents

**Required reading:**
- [04 - Incremental Computation](../docs/04-incremental-computation.md) - Core query architecture
- [10 - Performance Engineering](../docs/10-performance-engineering.md) - Caching, persistence
- [03 - Architecture Overview](../docs/03-architecture-overview.md) - System design
- [19 - Database Interfaces](../docs/19-database-interfaces.md) - Query DB design

**ADRs:**
- [ADR-0001: Incremental Query Engine](../docs/adr/0001-incremental-query-engine.md)
- [ADR-0004: Concurrency Model](../docs/adr/0004-concurrency-model.md)
- [ADR-0005: Persistence Formats](../docs/adr/0005-persistence-formats.md)

---

## Development Guidelines

### Query Database (nova-db)

The query database is the heart of Nova. All data flows through Salsa queries.

```rust
// Input queries - set externally
#[salsa::input]
fn file_content(&self, file: FileId) -> Arc<str>;

// Derived queries - computed and memoized
#[salsa::tracked]
fn parse_file(&self, file: FileId) -> Arc<ParseResult>;
```

**Rules:**
1. Never bypass the query system for memoizable data
2. Keep query granularity appropriate (not too fine, not too coarse)
3. Use `Arc` for large returned values to avoid cloning
4. Mark queries `#[salsa::tracked]` unless they're inputs

### Virtual File System (nova-vfs)

The VFS abstracts file access and provides overlay support for unsaved editor buffers.

**Key invariants:**
1. `FileId` must be stable across renames of open documents
2. Overlay (editor buffer) always takes precedence over disk
3. All paths must be canonicalized for consistent identity
4. Archive files (JARs) are accessed through the VFS

**Cross-platform concerns:**
```rust
// GOOD: Canonicalize temp paths in tests
let root = dir.path().canonicalize().unwrap();

// BAD: Assumes paths match without canonicalization
let root = dir.path();  // /var vs /private/var on macOS
```

### Memory Management (nova-memory)

Nova must work within memory constraints. The memory subsystem provides:
- Budget allocation across cache categories
- Eviction policies when pressure is high
- Integration with RLIMIT_AS

**Rules:**
1. Large caches must register with `MemoryBudget`
2. Respect eviction signals - shed load gracefully
3. Use streaming/iterators instead of collecting large results
4. Test with constrained memory budgets

### Caching (nova-cache)

```rust
// Bounded LRU cache with memory tracking
let cache = BoundedCache::new(budget_category);
cache.insert(key, value);

// Derived value cache with persistence
let derived = DerivedCache::new(storage_path);
```

**Rules:**
1. All caches must be bounded
2. Cache keys should be cheap to hash
3. Consider persistence for expensive-to-compute values
4. Document cache invalidation strategy

---

## Testing

```bash
# Test core crates
bash scripts/cargo_agent.sh test -p nova-core --lib
bash scripts/cargo_agent.sh test -p nova-db --lib
bash scripts/cargo_agent.sh test -p nova-vfs --lib
bash scripts/cargo_agent.sh test -p nova-cache --lib
bash scripts/cargo_agent.sh test -p nova-memory --lib
bash scripts/cargo_agent.sh test -p nova-workspace --lib
```

**Test patterns:**
- Test query incrementality (change input, verify minimal recomputation)
- Test VFS overlay precedence
- Test memory pressure handling
- Test cross-platform path behavior

---

## Common Pitfalls

1. **Forgetting to canonicalize paths** - Causes test failures on macOS
2. **Query cycles** - Salsa will panic; design query dependencies carefully
3. **Unbounded caches** - Will OOM under load
4. **Blocking in async context** - Use appropriate async primitives

---

## Dependencies

This workstream is a dependency for all other workstreams. Changes here have wide impact.

**Downstream dependents:** All other workstreams
**Upstream dependencies:** None (this is the foundation)

---

## Coordination

When making breaking changes to core types or query signatures:
1. Announce in advance
2. Update all downstream crates atomically
3. Update relevant documentation

---

*Remember: Always follow [AGENTS.md](../AGENTS.md) rules. Use wrapper scripts. Scope your cargo commands.*
