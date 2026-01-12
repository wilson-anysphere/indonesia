# 03 - Architecture Overview

[← Back to Main Document](../AGENTS.md) | [Previous: Current Landscape](02-current-landscape.md)

## Overview

This document describes Nova's high-level architecture—the system design that enables us to surpass IntelliJ. The architecture is built around three core principles: **query-based incremental computation**, **layered abstraction**, and **composable components**.

**Implementation note:** Binding technology choices (incremental engine, syntax trees, protocols, persistence, etc.) are tracked as ADRs. See [`docs/architecture.md`](architecture.md).

---

## System Architecture

```
┌─────────────────────────────────────────────────────────────────────────┐
│                           NOVA ARCHITECTURE                              │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                          │
│  ┌────────────────────────────────────────────────────────────────┐     │
│  │                      CLIENT LAYER                               │     │
│  │  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────────────┐   │     │
│  │  │ VS Code  │  │   Vim    │  │  Emacs   │  │ Custom Tools │   │     │
│  │  └────┬─────┘  └────┬─────┘  └────┬─────┘  └──────┬───────┘   │     │
│  │       │             │             │               │            │     │
│  │       └─────────────┴──────┬──────┴───────────────┘            │     │
│  └────────────────────────────┼───────────────────────────────────┘     │
│                               │ LSP/DAP/Custom Protocols                 │
│                               ▼                                          │
│  ┌────────────────────────────────────────────────────────────────┐     │
│  │                    PROTOCOL LAYER                               │     │
│  │  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────┐    │     │
│  │  │ LSP Server  │  │ DAP Server  │  │  Extension Server   │    │     │
│  │  └──────┬──────┘  └──────┬──────┘  └──────────┬──────────┘    │     │
│  │         │                │                    │                │     │
│  │         └────────────────┴──────────┬─────────┘                │     │
│  └──────────────────────────────────────┼─────────────────────────┘     │
│                                         ▼                                │
│  ┌────────────────────────────────────────────────────────────────┐     │
│  │                   INTELLIGENCE LAYER                            │     │
│  │                                                                 │     │
│  │  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────┐    │     │
│  │  │ Completions │  │ Diagnostics │  │    Code Actions     │    │     │
│  │  └─────────────┘  └─────────────┘  └─────────────────────┘    │     │
│  │  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────┐    │     │
│  │  │ Navigation  │  │ Refactoring │  │ Framework Analyzers │    │     │
│  │  └─────────────┘  └─────────────┘  └─────────────────────┘    │     │
│  │                           │                                    │     │
│  └───────────────────────────┼────────────────────────────────────┘     │
│                              ▼                                           │
│  ┌────────────────────────────────────────────────────────────────┐     │
│  │                   SEMANTIC LAYER                                │     │
│  │                                                                 │     │
│  │  ┌──────────────┐  ┌────────────────┐  ┌─────────────────┐    │     │
│  │  │ Type System  │  │   Resolution   │  │   Inference     │    │     │
│  │  └──────────────┘  └────────────────┘  └─────────────────┘    │     │
│  │  ┌──────────────┐  ┌────────────────┐  ┌─────────────────┐    │     │
│  │  │   Scopes     │  │    Symbols     │  │    Bindings     │    │     │
│  │  └──────────────┘  └────────────────┘  └─────────────────┘    │     │
│  │                           │                                    │     │
│  └───────────────────────────┼────────────────────────────────────┘     │
│                              ▼                                           │
│  ┌────────────────────────────────────────────────────────────────┐     │
│  │                    SYNTAX LAYER                                 │     │
│  │                                                                 │     │
│  │  ┌──────────────┐  ┌────────────────┐  ┌─────────────────┐    │     │
│  │  │    Lexer     │  │     Parser     │  │   Syntax Trees  │    │     │
│  │  └──────────────┘  └────────────────┘  └─────────────────┘    │     │
│  │  ┌──────────────┐  ┌────────────────┐                         │     │
│  │  │Error Recovery│  │  Incremental   │                         │     │
│  │  └──────────────┘  └────────────────┘                         │     │
│  │                           │                                    │     │
│  └───────────────────────────┼────────────────────────────────────┘     │
│                              ▼                                           │
│  ┌────────────────────────────────────────────────────────────────┐     │
│  │                 FOUNDATION LAYER                                │     │
│  │                                                                 │     │
│  │  ┌──────────────────────────────────────────────────────┐     │     │
│  │  │              QUERY DATABASE (Salsa-based)             │     │     │
│  │  │  • Memoization  • Dependency tracking  • Invalidation │     │     │
│  │  └──────────────────────────────────────────────────────┘     │     │
│  │  ┌──────────────┐  ┌────────────────┐  ┌─────────────────┐    │     │
│  │  │    Files     │  │     Index      │  │   Persistence   │    │     │
│  │  └──────────────┘  └────────────────┘  └─────────────────┘    │     │
│  │                                                                │     │
│  └────────────────────────────────────────────────────────────────┘     │
│                                                                          │
└─────────────────────────────────────────────────────────────────────────┘
```

---

## Layer Descriptions

### Foundation Layer

The foundation provides the core primitives that all other layers build upon.

#### Query Database

The heart of Nova. Every computation is a query:

```rust
// Conceptual query definitions (not actual implementation syntax)

// Input query: raw file content
#[query]
fn file_content(db: &dyn Database, file: FileId) -> Arc<String>;

// Derived query: parse file into syntax tree
#[query]
fn parse(db: &dyn Database, file: FileId) -> Arc<SyntaxTree>;

// Derived query: resolve all symbols in a file
#[query]
fn file_symbols(db: &dyn Database, file: FileId) -> Arc<SymbolTable>;

// Derived query: type-check an expression
#[query]
fn type_of(db: &dyn Database, expr: ExprId) -> Type;
```

**Properties:**
- **Memoization**: Query results cached automatically
- **Dependency Tracking**: System knows which queries depend on which
- **Invalidation**: Change a file → only affected queries re-run
- **Parallelism**: Independent queries can run in parallel

See [04 - Incremental Computation](04-incremental-computation.md) for deep dive.

#### File System Abstraction

```
┌─────────────────────────────────────────────────────────────────┐
│                    File System Layer                             │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │                  Virtual File System                     │    │
│  │  • Unified interface for all file access                │    │
│  │  • Handles local files, JARs, remote files              │    │
│  │  • Change notification integration                      │    │
│  └─────────────────────────────────────────────────────────┘    │
│                              │                                   │
│          ┌───────────────────┼───────────────────┐              │
│          ▼                   ▼                   ▼              │
│  ┌──────────────┐  ┌──────────────┐  ┌────────────────┐        │
│  │  Local FS    │  │  JAR Reader  │  │  Remote FS     │        │
│  └──────────────┘  └──────────────┘  └────────────────┘        │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

#### Index System

Persistent indexes for fast global queries:

```
┌─────────────────────────────────────────────────────────────────┐
│                     Index Types                                  │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  SYMBOL INDEX                                                    │
│  Key: Symbol name (string)                                      │
│  Value: List of (FileId, Location) where symbol is defined      │
│  Use: "Go to Symbol", "Find Class"                              │
│                                                                  │
│  REFERENCE INDEX                                                 │
│  Key: Qualified symbol name                                     │
│  Value: List of (FileId, Location) where symbol is used         │
│  Use: "Find References", candidate filtering                    │
│                                                                  │
│  INHERITANCE INDEX                                               │
│  Key: Class/Interface name                                      │
│  Value: List of subclasses/implementors                         │
│  Use: "Find Implementations", type hierarchy                    │
│                                                                  │
│  ANNOTATION INDEX                                                │
│  Key: Annotation type                                           │
│  Value: List of annotated elements                              │
│  Use: Framework support, Spring beans, etc.                     │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Syntax Layer

Parsing and syntax tree management.

See [05 - Syntax and Parsing](05-syntax-and-parsing.md) for detailed design.

#### Key Design Decisions

1. **Lossless Syntax Trees**: Preserve all source text including whitespace and comments
2. **Error-Resilient Parsing**: Always produce a tree, even with errors
3. **Incremental Reparsing**: Only reparse changed regions
4. **Immutable Trees**: Trees are immutable; edits create new trees sharing unchanged nodes

#### Syntax Tree Structure

```
┌─────────────────────────────────────────────────────────────────┐
│                    Syntax Tree Design                            │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  GREEN TREE (Immutable, position-independent)                   │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │  • Tree of syntax nodes and tokens                      │    │
│  │  • Each node has: kind, children, text length           │    │
│  │  • No absolute positions (enables sharing)              │    │
│  │  • Reference-counted for memory efficiency              │    │
│  └─────────────────────────────────────────────────────────┘    │
│                              │                                   │
│                              ▼                                   │
│  RED TREE (Position-aware wrapper)                              │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │  • Wraps green nodes with position info                 │    │
│  │  • Created on-demand during traversal                   │    │
│  │  • Enables: parent pointers, absolute positions         │    │
│  │  • Cheap to create (just a pointer + offset)            │    │
│  └─────────────────────────────────────────────────────────┘    │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Semantic Layer

Type checking, resolution, and semantic analysis.

See [06 - Semantic Analysis](06-semantic-analysis.md) for detailed design.
See also: stable type identity via `ClassId` ([ADR 0011](adr/0011-stable-classid-and-project-type-environments.md), [ADR 0012](adr/0012-classid-interning.md)).

#### Components

```
┌─────────────────────────────────────────────────────────────────┐
│                   Semantic Analysis Pipeline                     │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  1. NAME RESOLUTION                                              │
│     • Resolve identifiers to declarations                       │
│     • Handle imports, static imports, star imports              │
│     • Scope walking for local variables                         │
│                                                                  │
│  2. TYPE RESOLUTION                                              │
│     • Resolve type names to type definitions                    │
│     • Handle generics, arrays, wildcards                        │
│     • Process type annotations                                  │
│                                                                  │
│  3. TYPE CHECKING                                                │
│     • Verify type compatibility                                 │
│     • Check method call validity                                │
│     • Validate assignments, casts                               │
│                                                                  │
│  4. TYPE INFERENCE                                               │
│     • Infer types for var declarations                          │
│     • Infer lambda parameter types                              │
│     • Infer generic type arguments                              │
│                                                                  │
│  5. FLOW ANALYSIS                                                │
│     • Definite assignment analysis                              │
│     • Reachability analysis                                     │
│     • Exception flow                                            │
│     • Null flow analysis                                        │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Intelligence Layer

User-facing features built on the semantic layer.

See documents:
- [07 - Code Intelligence](07-code-intelligence.md)
- [08 - Refactoring Engine](08-refactoring-engine.md)

#### Feature Categories

```
┌─────────────────────────────────────────────────────────────────┐
│                    Intelligence Features                         │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  COMPLETIONS                                                     │
│  • Context-aware completion                                     │
│  • Import suggestions                                           │
│  • Postfix completion                                           │
│  • Live templates                                               │
│  • AI-augmented suggestions                                     │
│                                                                  │
│  DIAGNOSTICS                                                     │
│  • Compilation errors                                           │
│  • Warnings                                                     │
│  • Code style issues                                            │
│  • Custom inspections                                           │
│  • Framework-specific checks                                    │
│                                                                  │
│  NAVIGATION                                                      │
│  • Go to definition                                             │
│  • Find references                                              │
│  • Type hierarchy                                               │
│  • Call hierarchy                                               │
│  • Implementation search                                        │
│                                                                  │
│  REFACTORING                                                     │
│  • Rename                                                       │
│  • Extract (method, variable, constant, parameter)              │
│  • Inline                                                       │
│  • Move                                                         │
│  • Change signature                                             │
│                                                                  │
│  CODE ACTIONS                                                    │
│  • Quick fixes for errors                                       │
│  • Intention actions                                            │
│  • Generate code (getters, constructors, etc.)                  │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Protocol Layer

Communication with editors and tools.

See [11 - Editor Integration](11-editor-integration.md) for detailed design.

```
┌─────────────────────────────────────────────────────────────────┐
│                    Protocol Layer                                │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  LSP SERVER                                                      │
│  • Full LSP 3.17+ implementation                                │
│  • Java-specific extensions                                     │
│  • Request cancellation                                         │
│  • Progress reporting                                           │
│                                                                  │
│  DAP SERVER                                                      │
│  • Debug Adapter Protocol implementation                        │
│  • JVM debugger integration                                     │
│  • Hot code replacement                                         │
│                                                                  │
│  EXTENSION PROTOCOL                                              │
│  • Custom Java-specific messages                                │
│  • Project configuration                                        │
│  • Build integration                                            │
│  • Test discovery                                               │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

---

## Data Flow Examples

### Example 1: File Edit → Diagnostics

```
User types character
        │
        ▼
┌─────────────────┐
│ LSP: didChange  │
└────────┬────────┘
         │
         ▼
┌─────────────────┐
│ Update file     │
│ content query   │
└────────┬────────┘
         │  (invalidates dependent queries)
         ▼
┌─────────────────┐
│ Reparse file    │──────┐
│ (incremental)   │      │ (if syntax changed)
└────────┬────────┘      │
         │               │
         ▼               ▼
┌─────────────────┐ ┌─────────────────┐
│ Re-resolve      │ │ Re-type-check   │
│ symbols         │ │ affected exprs  │
└────────┬────────┘ └────────┬────────┘
         │                   │
         └─────────┬─────────┘
                   │
                   ▼
         ┌─────────────────┐
         │ Compute         │
         │ diagnostics     │
         └────────┬────────┘
                  │
                  ▼
         ┌─────────────────┐
         │ LSP: publish    │
         │ Diagnostics     │
         └─────────────────┘
```

### Example 2: Completion Request

```
User triggers completion at position
                │
                ▼
       ┌─────────────────┐
       │ LSP: completion │
       └────────┬────────┘
                │
                ▼
       ┌─────────────────┐
       │ Determine       │
       │ completion      │
       │ context         │
       └────────┬────────┘
                │
        ┌───────┴───────┐
        ▼               ▼
┌──────────────┐ ┌──────────────┐
│ Expression   │ │  Statement   │
│ context      │ │  context     │
└──────┬───────┘ └──────┬───────┘
       │                │
       ▼                ▼
┌──────────────┐ ┌──────────────┐
│ Get type at  │ │ Get visible  │
│ receiver     │ │ symbols      │
└──────┬───────┘ └──────┬───────┘
       │                │
       ▼                ▼
┌──────────────┐ ┌──────────────┐
│ Get members  │ │ Filter by    │
│ of type      │ │ prefix       │
└──────┬───────┘ └──────┬───────┘
       │                │
       └───────┬────────┘
               │
               ▼
       ┌─────────────────┐
       │ Rank and sort   │
       │ completions     │
       └────────┬────────┘
               │
               ▼
       ┌─────────────────┐
       │ LSP: completion │
       │ response        │
       └─────────────────┘
```

---

## Component Interfaces

### Database Trait

```rust
use std::path::Path;

use nova_db::FileId;

/// In code, Nova’s incremental query engine lives in `crates/nova-db/src/salsa/` as
/// `nova_db::salsa::RootDatabase` (ADR 0001).
///
/// For consumers that only need file text, `nova-db` also exposes a lightweight
/// trait:
pub trait Database {
    fn file_content(&self, file_id: FileId) -> &str;

    fn file_path(&self, _file_id: FileId) -> Option<&Path> {
        None
    }

    fn all_file_ids(&self) -> Vec<FileId> {
        Vec::new()
    }

    fn file_id(&self, _path: &Path) -> Option<FileId> {
        None
    }
}
```

### Query Groups

Queries are organized into logical groups:

```rust
/// Syntax-level queries
#[ra_salsa::query_group(SyntaxDatabaseStorage)]
pub trait SyntaxDatabase {
    #[ra_salsa::input]
    fn file_content(&self, file: FileId) -> Arc<String>;
    
    fn parse(&self, file: FileId) -> Arc<ParseResult>;
    fn syntax_tree(&self, file: FileId) -> Arc<SyntaxTree>;
}

/// Semantic queries
#[ra_salsa::query_group(SemanticDatabaseStorage)]  
pub trait SemanticDatabase: SyntaxDatabase {
    fn resolve_imports(&self, file: FileId) -> Arc<ImportMap>;
    fn file_symbols(&self, file: FileId) -> Arc<SymbolTable>;
    fn type_of(&self, file: FileId, expr: ExprId) -> TypeResult;
}

/// Intelligence queries
#[ra_salsa::query_group(IntelligenceDatabaseStorage)]
pub trait IntelligenceDatabase: SemanticDatabase {
    fn completions_at(&self, file: FileId, pos: Position) -> Vec<CompletionItem>;
    fn diagnostics(&self, file: FileId) -> Vec<Diagnostic>;
    fn hover(&self, file: FileId, pos: Position) -> Option<HoverInfo>;
}
```

---

## Concurrency Model

### Read-Write Separation

```
┌─────────────────────────────────────────────────────────────────┐
│                    Concurrency Model                             │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  WRITE OPERATIONS (Single Writer)                               │
│  • File content updates                                         │
│  • Configuration changes                                        │
│  • Index updates                                                │
│  • Serialized through single entry point                        │
│                                                                  │
│  READ OPERATIONS (Multiple Readers)                             │
│  • Query execution                                              │
│  • Can run in parallel                                          │
│  • Use snapshot of database state                               │
│                                                                  │
│  BACKGROUND TASKS                                                │
│  • Index building                                               │
│  • Diagnostic computation                                       │
│  • Can be cancelled and restarted                               │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Request Handling

```rust
// Conceptual request handling flow
async fn handle_request(&self, request: LspRequest) -> LspResponse {
    // Get database snapshot
    let snapshot = self.db.snapshot();
    
    // Run query on snapshot (can be parallel with other requests)
    let result = match request {
        LspRequest::Completion(params) => {
            let completions = snapshot.completions_at(
                params.file,
                params.position
            );
            LspResponse::Completion(completions)
        }
        // ... other request types
    };
    
    result
}
```

---

## Extension Points

### Plugin Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                    Extension System                              │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  FRAMEWORK ANALYZERS                                             │
│  • Spring analyzer                                              │
│  • Jakarta EE analyzer                                          │
│  • Custom framework support                                     │
│                                                                  │
│  DIAGNOSTIC PROVIDERS                                            │
│  • Additional inspections                                       │
│  • Code style checks                                            │
│  • Security analyzers                                           │
│                                                                  │
│  COMPLETION PROVIDERS                                            │
│  • Framework-specific completions                               │
│  • Custom snippets                                              │
│  • AI providers                                                 │
│                                                                  │
│  CODE ACTION PROVIDERS                                           │
│  • Custom quick fixes                                           │
│  • Refactoring extensions                                       │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Extension Interface

```rust
use nova_core::ProjectId;
use nova_framework::{
    CompletionContext, FrameworkData, InlayHint, NavigationTarget, Symbol, VirtualMember,
};
use nova_scheduler::CancellationToken;
use nova_types::{ClassId, CompletionItem, Diagnostic};
use nova_vfs::FileId;

/// Extension point for framework analyzers (see `crates/nova-framework/src/lib.rs`).
 pub trait FrameworkAnalyzer: Send + Sync {
    fn applies_to(&self, db: &dyn nova_framework::Database, project: ProjectId) -> bool;

    fn analyze_file(
        &self,
        _db: &dyn nova_framework::Database,
        _file: FileId,
    ) -> Option<FrameworkData> {
        None
    }

    fn diagnostics(&self, _db: &dyn nova_framework::Database, _file: FileId) -> Vec<Diagnostic> {
        Vec::new()
    }

    fn diagnostics_with_cancel(
        &self,
        db: &dyn nova_framework::Database,
        file: FileId,
        cancel: &CancellationToken,
    ) -> Vec<Diagnostic> {
        if cancel.is_cancelled() {
            Vec::new()
        } else {
            self.diagnostics(db, file)
        }
    }

    fn completions(
        &self,
        _db: &dyn nova_framework::Database,
        _ctx: &CompletionContext,
    ) -> Vec<CompletionItem> {
        Vec::new()
    }

    fn completions_with_cancel(
        &self,
        db: &dyn nova_framework::Database,
        ctx: &CompletionContext,
        cancel: &CancellationToken,
    ) -> Vec<CompletionItem> {
        if cancel.is_cancelled() {
            Vec::new()
        } else {
            self.completions(db, ctx)
        }
    }

    fn navigation(
        &self,
        _db: &dyn nova_framework::Database,
        _symbol: &Symbol,
    ) -> Vec<NavigationTarget> {
        Vec::new()
    }

    fn navigation_with_cancel(
        &self,
        db: &dyn nova_framework::Database,
        symbol: &Symbol,
        cancel: &CancellationToken,
    ) -> Vec<NavigationTarget> {
        if cancel.is_cancelled() {
            Vec::new()
        } else {
            self.navigation(db, symbol)
        }
    }

    fn virtual_members(
        &self,
        _db: &dyn nova_framework::Database,
        _class: ClassId,
    ) -> Vec<VirtualMember> {
        Vec::new()
    }

    fn inlay_hints(&self, _db: &dyn nova_framework::Database, _file: FileId) -> Vec<InlayHint> {
        Vec::new()
    }

    fn inlay_hints_with_cancel(
        &self,
        db: &dyn nova_framework::Database,
        file: FileId,
        cancel: &CancellationToken,
    ) -> Vec<InlayHint> {
        if cancel.is_cancelled() {
            Vec::new()
        } else {
            self.inlay_hints(db, file)
        }
    }
}
```

 ---

 ## Memory Management

### Strategy

```
┌─────────────────────────────────────────────────────────────────┐
│                    Memory Strategy                               │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  HOT DATA (In memory)                                           │
│  • Currently open files' syntax trees                           │
│  • Recent query results                                         │
│  • Active type information                                      │
│                                                                  │
│  WARM DATA (Memory-mapped)                                       │
│  • Project-wide indexes                                         │
│  • Dependency type information                                  │
│  • Cached analysis results                                      │
│                                                                  │
│  COLD DATA (On disk)                                            │
│  • Closed file analysis                                         │
│  • Full dependency info                                         │
│  • Historical data                                              │
│                                                                  │
│  EVICTION POLICY                                                 │
│  • LRU for query cache                                          │
│  • Reference counting for syntax trees                          │
│  • Proactive eviction under memory pressure                     │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

---

## Next Steps

For detailed design of each subsystem:

1. → [Incremental Computation](04-incremental-computation.md): Query database design
2. → [Syntax and Parsing](05-syntax-and-parsing.md): Parser architecture
3. → [Semantic Analysis](06-semantic-analysis.md): Type system implementation

---

[← Previous: Current Landscape](02-current-landscape.md) | [Next: Incremental Computation →](04-incremental-computation.md)
