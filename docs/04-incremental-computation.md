# 04 - Incremental Computation Engine

[← Back to Main Document](../AGENTS.md) | [Previous: Architecture Overview](03-architecture-overview.md)

## Overview

The incremental computation engine is **the core innovation** that enables Nova to surpass IntelliJ. This document describes the query-based architecture that makes true incremental analysis possible.

**Implementation note:** Nova’s incremental query engine is implemented with Salsa via rust-analyzer’s `ra_ap_salsa` crate (imported as `ra_salsa`) in `crates/nova-db` (see [ADR 0001](adr/0001-incremental-query-engine.md)). The code snippets below are illustrative; the concrete macro names/types follow `ra_salsa::*`.

---

## The Problem with Traditional Approaches

### Batch Compilation Model

Traditional compilers (including javac) use a batch model:

```
┌─────────────────────────────────────────────────────────────────┐
│                   BATCH MODEL (Traditional)                      │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  Input: Source files                                            │
│           │                                                      │
│           ▼                                                      │
│  ┌─────────────────┐                                            │
│  │   Parse ALL     │ ──────────────────────────────────────┐    │
│  │   files         │                                       │    │
│  └────────┬────────┘                                       │    │
│           │                                                │    │
│           ▼                                                │    │
│  ┌─────────────────┐                                       │    │
│  │  Resolve ALL    │                                       │    │
│  │  symbols        │                                       │    │
│  └────────┬────────┘                                       │    │
│           │                                            FULL     │
│           ▼                                          REBUILD    │
│  ┌─────────────────┐                               ON EVERY     │
│  │  Type-check ALL │                                 CHANGE     │
│  │  expressions    │                                       │    │
│  └────────┬────────┘                                       │    │
│           │                                                │    │
│           ▼                                                │    │
│  ┌─────────────────┐                                       │    │
│  │  Output: All    │ ◄─────────────────────────────────────┘    │
│  │  results        │                                            │
│  └─────────────────┘                                            │
│                                                                  │
│  Problem: Change one file → recompute everything                │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### "Incremental" But Not Really

Many tools claim to be incremental but only at file granularity:

```
┌─────────────────────────────────────────────────────────────────┐
│              FILE-LEVEL "INCREMENTAL" (JDT, etc.)               │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  Change in Foo.java                                             │
│           │                                                      │
│           ▼                                                      │
│  ┌─────────────────┐                                            │
│  │  Fully reparse  │                                            │
│  │  Foo.java       │                                            │
│  └────────┬────────┘                                            │
│           │                                                      │
│           ▼                                                      │
│  ┌─────────────────┐                                            │
│  │  Fully resolve  │ ◄── Still wasteful if change               │
│  │  Foo.java       │     is isolated                            │
│  └────────┬────────┘                                            │
│           │                                                      │
│           ▼                                                      │
│  ┌─────────────────┐                                            │
│  │  Fully check    │ ◄── Recheck entire file even               │
│  │  Foo.java       │     if change is one method                │
│  └────────┬────────┘                                            │
│           │                                                      │
│           ▼                                                      │
│  ┌─────────────────┐                                            │
│  │  Re-resolve     │ ◄── Must also re-resolve files             │
│  │  dependents     │     that reference Foo                     │
│  └─────────────────┘                                            │
│                                                                  │
│  Problem: File-level granularity is too coarse                  │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

---

## Nova's Query-Based Model

### The Insight

Every piece of information in an IDE can be expressed as a **query**:

- "What is the syntax tree of file X?" → Query
- "What does identifier Y resolve to?" → Query  
- "What is the type of expression Z?" → Query
- "What completions are valid at position P?" → Query

Queries have dependencies:
- `type_of(expr)` depends on `resolve(expr.receiver)` and `lookup_method(...)`
- `resolve(ident)` depends on `imports(file)` and `scope_at(position)`

### Query Database Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                    QUERY DATABASE                                │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  INPUT QUERIES (Set externally)                                 │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │  file_content(FileId) → String                          │    │
│  │  file_exists(FileId) → bool                             │    │
│  │  config() → ProjectConfig                               │    │
│  └─────────────────────────────────────────────────────────┘    │
│                              │                                   │
│                              │ depend on                         │
│                              ▼                                   │
│  DERIVED QUERIES (Computed from other queries)                  │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │  parse(FileId) → SyntaxTree                             │    │
│  │    └── depends on: file_content(file_id)                │    │
│  │                                                         │    │
│  │  imports(FileId) → ImportMap                            │    │
│  │    └── depends on: parse(file_id)                       │    │
│  │                                                         │    │
│  │  resolve(FileId, ExprId) → Symbol                       │    │
│  │    └── depends on: parse(file), imports(file),          │    │
│  │                    file_symbols(dependency_files...)     │    │
│  │                                                         │    │
│  │  type_of(FileId, ExprId) → Type                         │    │
│  │    └── depends on: resolve(...), type_of(subexprs...)   │    │
│  └─────────────────────────────────────────────────────────┘    │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Automatic Dependency Tracking

The magic: **dependencies are tracked automatically** during query execution.

```rust
// When this query runs...
fn type_of(db: &dyn Database, file: FileId, expr: ExprId) -> Type {
    let tree = db.parse(file);        // Records dependency on parse(file)
    let expr_node = tree.get(expr);
    
    match expr_node {
        Expr::MethodCall { receiver, method, args } => {
            let recv_type = db.type_of(file, receiver);  // Dependency recorded
            let method_sym = db.resolve_method(recv_type, method);  // Dependency
            // ...
        }
        // ...
    }
}
```

The database automatically tracks that `type_of(file, expr)` depends on:
- `parse(file)`
- `type_of(file, receiver)` 
- `resolve_method(recv_type, method)`
- (and whatever those queries depend on, transitively)

### Invalidation and Recomputation

When an input changes, only affected queries are recomputed:

```
┌─────────────────────────────────────────────────────────────────┐
│              INCREMENTAL RECOMPUTATION                           │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  User edits Foo.java (changes method body)                      │
│                                                                  │
│  1. INVALIDATE DIRECTLY AFFECTED                                │
│     file_content("Foo.java") ← new value                        │
│                                                                  │
│  2. MARK DEPENDENTS STALE (not computed yet)                    │
│     parse("Foo.java") ← stale                                   │
│     file_symbols("Foo.java") ← stale                            │
│     type_of("Foo.java", ...) ← stale                            │
│     diagnostics("Foo.java") ← stale                             │
│                                                                  │
│  3. ON NEXT ACCESS, VERIFY OR RECOMPUTE                         │
│     parse("Foo.java"):                                          │
│       - Input changed → must recompute                          │
│       - Parse file                                              │
│       - Compare result to cached: syntax tree SAME              │
│       - Mark dependents as VERIFIED (not stale)                 │
│                                                                  │
│  4. RESULT: MINIMAL WORK                                        │
│     - If method body change doesn't affect signatures           │
│     - No type-checking needed for other files!                  │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

---

## Implementation Details

### Query Definition

```rust
/// Example query definitions using salsa-like patterns

// Input query: externally set
#[query(input)]
pub fn file_content(db: &dyn Database, file: FileId) -> Arc<String>;

// Derived query: computed from other queries
#[query]
pub fn parse(db: &dyn Database, file: FileId) -> Arc<ParseResult> {
    let content = db.file_content(file);
    let lexer = Lexer::new(&content);
    let parser = Parser::new(lexer);
    Arc::new(parser.parse())
}

// Query with multiple inputs
#[query]
pub fn resolve_name(
    db: &dyn Database,
    file: FileId,
    scope: ScopeId,
    name: Name
) -> Option<Symbol> {
    // Look in local scope
    let scope_data = db.scope(file, scope);
    if let Some(sym) = scope_data.lookup(&name) {
        return Some(sym);
    }
    
    // Look in imports
    let imports = db.imports(file);
    if let Some(sym) = imports.resolve(&name) {
        return Some(sym);
    }
    
    // Look in java.lang implicit imports
    db.java_lang_symbols().get(&name)
}
```

### Memoization Strategy

```
┌─────────────────────────────────────────────────────────────────┐
│                    MEMOIZATION STRATEGY                          │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  QUERY CACHE STRUCTURE                                          │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │  Key: (QueryType, Arg1, Arg2, ...)                      │    │
│  │  Value: {                                                │    │
│  │    result: Arc<QueryResult>,                            │    │
│  │    dependencies: Vec<QueryKey>,                         │    │
│  │    changed_at: Revision,                                │    │
│  │    verified_at: Revision,                               │    │
│  │  }                                                       │    │
│  └─────────────────────────────────────────────────────────┘    │
│                                                                  │
│  REVISION TRACKING                                               │
│  • Global revision counter                                      │
│  • Incremented on any input change                              │
│  • Queries track when they were last verified                   │
│                                                                  │
│  VERIFICATION ALGORITHM                                          │
│  1. If verified_at == current_revision: return cached           │
│  2. For each dependency:                                        │
│       - Recursively verify dependency                           │
│       - If dependency value changed: recompute self             │
│  3. If all deps unchanged: mark verified, return cached         │
│  4. If any dep changed: recompute, cache new result             │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Granularity Levels

Nova uses different granularity levels for different queries:

```
┌─────────────────────────────────────────────────────────────────┐
│                    QUERY GRANULARITY                             │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  FILE-LEVEL QUERIES                                              │
│  • parse(file) - syntax tree for entire file                   │
│  • imports(file) - import statements                           │
│  • file_structure(file) - class/method outline                 │
│  └── Appropriate: parsing, imports are file-coherent           │
│                                                                  │
│  DECLARATION-LEVEL QUERIES                                       │
│  • method_signature(method_id) - return type, params           │
│  • field_type(field_id) - declared type                        │
│  • class_supertypes(class_id) - extends/implements             │
│  └── Appropriate: signatures change independently              │
│                                                                  │
│  EXPRESSION-LEVEL QUERIES                                        │
│  • type_of(expr_id) - type of specific expression              │
│  • resolve(expr_id) - what identifier refers to                │
│  └── Appropriate: maximum incrementality                       │
│                                                                  │
│  CHOOSING GRANULARITY:                                          │
│  • Finer = more incremental but more overhead                  │
│  • Coarser = less overhead but more recomputation              │
│  • Nova: fine-grained where it matters (types), coarse where   │
│    changes are coherent (imports, parsing)                     │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

---

## Concurrency Model

### Snapshot-Based Reads

```rust
/// Database supports concurrent read access through snapshots
pub trait Database: Send + Sync {
    /// Create a snapshot for read-only access
    /// Snapshots see a consistent view of the database
    fn snapshot(&self) -> Snapshot<Self>;
    
    /// Set an input query value (requires mutable access)
    fn set_file_text(&mut self, file: FileId, text: String);
}

/// Snapshot provides read-only access
pub struct Snapshot<DB> {
    db: Arc<DB>,
    revision: Revision,
}

impl<DB: Database> Snapshot<DB> {
    /// Execute a query on this snapshot
    pub fn query<Q: Query>(&self, args: Q::Args) -> Q::Result {
        // Uses revision for consistent view
        // Can run in parallel with other snapshots
    }
}
```

### Parallel Query Execution

```
┌─────────────────────────────────────────────────────────────────┐
│                    PARALLEL EXECUTION                            │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  SCENARIO: Multiple LSP requests arrive simultaneously          │
│                                                                  │
│  Request 1: Completion at file A                                │
│  Request 2: Hover at file B                                     │
│  Request 3: Diagnostics for file C                              │
│                                                                  │
│  ┌─────────────────┐  ┌─────────────────┐  ┌─────────────────┐  │
│  │    Thread 1     │  │    Thread 2     │  │    Thread 3     │  │
│  │  snapshot.      │  │  snapshot.      │  │  snapshot.      │  │
│  │  completions(A) │  │  hover(B)       │  │  diagnostics(C) │  │
│  └────────┬────────┘  └────────┬────────┘  └────────┬────────┘  │
│           │                    │                    │           │
│           ▼                    ▼                    ▼           │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │                  SHARED QUERY CACHE                      │    │
│  │  • Lock-free reads for cached results                   │    │
│  │  • Concurrent queries may share computation             │    │
│  │  • No blocking between independent queries              │    │
│  └─────────────────────────────────────────────────────────┘    │
│                                                                  │
│  INVARIANT: Snapshots see consistent state                      │
│  - Even if inputs change during execution                       │
│  - Snapshot isolated from concurrent writes                     │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Write Serialization

```rust
/// Writes are serialized through a single entry point
impl NovaServer {
    /// Handle file change notification
    fn on_did_change(&self, params: DidChangeParams) {
        // Acquire write lock (blocks other writes, not reads)
        let mut db = self.db.write();
        
        // Apply changes
        for change in params.changes {
            db.set_file_text(params.file, change.text);
        }
        
        // Release lock - snapshots see new state
    }
}
```

---

## Optimization Techniques

### Early Cutoff

If recomputation produces the same result, dependent queries don't need to recompute:

```
┌─────────────────────────────────────────────────────────────────┐
│                      EARLY CUTOFF                                │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  Scenario: Add whitespace in Foo.java                           │
│                                                                  │
│  1. file_content("Foo.java") → "class Foo { }" (changed)        │
│                                                                  │
│  2. parse("Foo.java"):                                          │
│     - Input changed, must reparse                               │
│     - New tree equals old tree (whitespace not in AST)          │
│     - EARLY CUTOFF: dependents don't recompute                  │
│                                                                  │
│  3. file_symbols("Foo.java"):                                   │
│     - Check: did parse() result change? NO                      │
│     - Return cached result without recomputing                  │
│                                                                  │
│  4. type_of(...), diagnostics(...), etc:                        │
│     - All skip recomputation due to cutoff                      │
│                                                                  │
│  Result: Whitespace edit = minimal work                         │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Demand-Driven Computation

Queries are only computed when needed:

```rust
// BAD: Eager computation (traditional approach)
fn analyze_project(project: &Project) {
    for file in project.files() {
        parse(file);
        resolve_all(file);
        type_check_all(file);  // Computed even if never used
    }
}

// GOOD: Demand-driven (Nova approach)
fn diagnostics_for(db: &dyn Database, file: FileId) -> Vec<Diagnostic> {
    // Only computes what's needed for diagnostics
    // If file has no errors, may not need full type-checking
    
    let tree = db.parse(file);  // Computed on first access, cached after
    
    // Only type-check expressions that could have errors
    let mut diagnostics = vec![];
    for expr in tree.expressions() {
        match db.type_of(file, expr.id()) {
            Type::Error(e) => diagnostics.push(e.into()),
            _ => {}
        }
    }
    diagnostics
}
```

### Stratified Invalidation

Not all queries invalidate the same way:

```
┌─────────────────────────────────────────────────────────────────┐
│                   STRATIFIED INVALIDATION                        │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  STRUCTURE-PRESERVING CHANGES                                   │
│  (e.g., change method body, not signature)                      │
│  └── Invalidates: type_of for that method's expressions         │
│  └── Preserves: file structure, method signatures, dependents   │
│                                                                  │
│  SIGNATURE CHANGES                                               │
│  (e.g., change method return type)                              │
│  └── Invalidates: method signature, callers' type checks        │
│  └── Preserves: file structure, other methods                   │
│                                                                  │
│  STRUCTURE CHANGES                                               │
│  (e.g., add/remove method)                                      │
│  └── Invalidates: file structure, resolution in dependents      │
│  └── Preserves: syntax trees of other files                     │
│                                                                  │
│  IMPORT CHANGES                                                  │
│  └── Invalidates: resolution for all identifiers in file        │
│  └── Preserves: syntax, other files                             │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

---

## Comparison with Other Systems

### vs. Salsa (rust-analyzer)

Nova's query system is inspired by Salsa but with Java-specific optimizations:

| Aspect | Salsa | Nova |
|--------|-------|------|
| Core model | Query-based | Query-based |
| Language | Rust | Rust |
| Granularity | Fine (per-function) | Adaptive |
| Interning | Extensive | Selective |
| Persistence | In-memory | Hybrid (memory + disk) |

### vs. IntelliJ PSI

| Aspect | IntelliJ PSI | Nova Queries |
|--------|--------------|--------------|
| Model | Mutable trees + caches | Immutable queries |
| Invalidation | Manual smart pointers | Automatic tracking |
| Concurrency | Complex threading | Snapshot-based |
| Persistence | Stubs + indexes | Query persistence |

### vs. Eclipse JDT

| Aspect | Eclipse JDT | Nova Queries |
|--------|-------------|--------------|
| Model | AST + bindings | Unified queries |
| Incrementality | File-level | Expression-level |
| Dependencies | Implicit | Explicit tracked |
| Recomputation | Manual triggers | Automatic |

---

## Advanced Topics

### Query Persistence

Queries can be persisted across sessions:

```rust
/// Persistent query cache
trait PersistentDatabase {
    /// Save query results to disk
    fn persist(&self, path: &Path) -> Result<()>;
    
    /// Load query results from disk
    fn load(&mut self, path: &Path) -> Result<()>;
    
    /// Queries marked as persistent will be saved
    #[query(persistent)]
    fn file_structure(db: &dyn Database, file: FileId) -> FileStructure;
}
```

### Distributed Queries

For very large codebases, queries can be distributed:

```
┌─────────────────────────────────────────────────────────────────┐
│                   DISTRIBUTED QUERIES                            │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  SCENARIO: Monorepo with millions of lines                      │
│                                                                  │
│  ┌─────────────────┐  ┌─────────────────┐                       │
│  │  Nova Instance  │  │  Nova Instance  │                       │
│  │  (Module A)     │  │  (Module B)     │                       │
│  └────────┬────────┘  └────────┬────────┘                       │
│           │                    │                                 │
│           └──────────┬─────────┘                                │
│                      │                                           │
│                      ▼                                           │
│           ┌─────────────────┐                                   │
│           │  Query Router   │                                   │
│           │  • Routes queries to appropriate instance           │
│           │  • Caches cross-module results                      │
│           │  • Handles invalidation propagation                 │
│           └─────────────────┘                                   │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

---

## Summary

The query-based incremental computation engine is what makes Nova fundamentally different from existing Java tools:

1. **Automatic Incrementality**: No manual invalidation logic
2. **Fine-Grained Updates**: Expression-level precision
3. **Concurrent Reads**: Snapshot-based parallelism
4. **Predictable Performance**: Same query = same cost
5. **Composable**: Queries build on queries

This foundation enables all of Nova's advanced features while maintaining performance that existing tools cannot match.

---

## Next Steps

1. → [Syntax and Parsing](05-syntax-and-parsing.md): How syntax trees integrate with queries
2. → [Semantic Analysis](06-semantic-analysis.md): How type checking uses queries

---

[← Previous: Architecture Overview](03-architecture-overview.md) | [Next: Syntax and Parsing →](05-syntax-and-parsing.md)
