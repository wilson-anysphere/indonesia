# 09 - Framework Support

[← Back to Main Document](../AGENTS.md) | [Previous: Refactoring Engine](08-refactoring-engine.md)

## Overview

Modern Java development is dominated by frameworks like Spring, Jakarta EE, and tools like Lombok. Deep framework support is where IntelliJ truly shines and where Nova must invest significantly. This document covers the approach to framework-aware analysis.

---

## Framework Support Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                FRAMEWORK SUPPORT ARCHITECTURE                    │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │                    CORE NOVA                             │    │
│  │  • Semantic analysis                                    │    │
│  │  • Type checking                                        │    │
│  │  • Symbol resolution                                    │    │
│  └───────────────────────┬─────────────────────────────────┘    │
│                          │                                       │
│                          ▼                                       │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │              FRAMEWORK ANALYZER INTERFACE                │    │
│  │  • Hooks into resolution                                │    │
│  │  • Provides additional completions                      │    │
│  │  • Adds framework-specific diagnostics                  │    │
│  │  • Extends navigation                                   │    │
│  └───────────────────────┬─────────────────────────────────┘    │
│                          │                                       │
│          ┌───────────────┼───────────────┬───────────────┐      │
│          ▼               ▼               ▼               ▼      │
│  ┌─────────────┐ ┌─────────────┐ ┌─────────────┐ ┌───────────┐  │
│  │   Spring    │ │  Jakarta    │ │   Lombok    │ │  Others   │  │
│  │  Analyzer   │ │  Analyzer   │ │  Analyzer   │ │  ...      │  │
│  └─────────────┘ └─────────────┘ └─────────────┘ └───────────┘  │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

---

## `nova-framework`: shared analyzer API

Framework crates that want to integrate with Nova’s generic “framework hooks” target the API in
`crates/nova-framework/src/lib.rs`.

It consists of:

- `nova_framework::Database`: minimal query surface a host must provide.
- `nova_framework::FrameworkAnalyzer`: optional hooks (virtual members, diagnostics, completions, …).
- `nova_framework::AnalyzerRegistry` (aka `FrameworkRegistry`): runs all applicable analyzers and
  aggregates their results.

### `nova_framework::Database` (real signature)

```rust
use std::path::Path;

use nova_core::ProjectId;
use nova_hir::framework::ClassData;
use nova_types::ClassId;
use nova_vfs::FileId;

pub trait Database {
    fn class(&self, class: ClassId) -> &ClassData;
    fn project_of_class(&self, class: ClassId) -> ProjectId;
    fn project_of_file(&self, file: FileId) -> ProjectId;

    fn file_text(&self, _file: FileId) -> Option<&str> {
        None
    }

    fn file_path(&self, _file: FileId) -> Option<&Path> {
        None
    }

    fn file_id(&self, _path: &Path) -> Option<FileId> {
        None
    }

    fn all_files(&self, _project: ProjectId) -> Vec<FileId> {
        Vec::new()
    }

    fn all_classes(&self, _project: ProjectId) -> Vec<ClassId> {
        Vec::new()
    }

    fn has_dependency(&self, project: ProjectId, group: &str, artifact: &str) -> bool;
    fn has_class_on_classpath(&self, project: ProjectId, binary_name: &str) -> bool;
    fn has_class_on_classpath_prefix(&self, project: ProjectId, prefix: &str) -> bool;
}
```

Optional methods (`file_text`, `file_path`, `file_id`, `all_files`, `all_classes`) may be
unimplemented by a host DB; analyzers should treat `None` / empty vectors as “information not
available” and skip cross-file scanning or file-text based features.

### `nova_framework::FrameworkAnalyzer` (real signature)

```rust
use nova_core::ProjectId;
use nova_framework::{
    CompletionContext, FrameworkData, InlayHint, NavigationTarget, Symbol, VirtualMember,
};
use nova_types::{ClassId, CompletionItem, Diagnostic};
use nova_vfs::FileId;

pub trait FrameworkAnalyzer: Send + Sync {
    fn applies_to(&self, db: &dyn Database, project: ProjectId) -> bool;

    fn analyze_file(&self, _db: &dyn Database, _file: FileId) -> Option<FrameworkData> {
        None
    }

    fn diagnostics(&self, _db: &dyn Database, _file: FileId) -> Vec<Diagnostic> {
        Vec::new()
    }

    fn completions(&self, _db: &dyn Database, _ctx: &CompletionContext) -> Vec<CompletionItem> {
        Vec::new()
    }

    fn navigation(&self, _db: &dyn Database, _symbol: &Symbol) -> Vec<NavigationTarget> {
        Vec::new()
    }

    fn virtual_members(&self, _db: &dyn Database, _class: ClassId) -> Vec<VirtualMember> {
        Vec::new()
    }

    fn inlay_hints(&self, _db: &dyn Database, _file: FileId) -> Vec<InlayHint> {
        Vec::new()
    }
}
```

### `AnalyzerRegistry` aggregation methods

```rust
use nova_framework::{
    AnalyzerRegistry, CompletionContext, Database, FrameworkData, InlayHint, NavigationTarget,
    Symbol, VirtualMember,
};
use nova_types::{ClassId, CompletionItem, Diagnostic};
use nova_vfs::FileId;

impl AnalyzerRegistry {
    pub fn framework_data(&self, db: &dyn Database, file: FileId) -> Vec<FrameworkData>;
    pub fn framework_diagnostics(&self, db: &dyn Database, file: FileId) -> Vec<Diagnostic>;
    pub fn framework_completions(
        &self,
        db: &dyn Database,
        ctx: &CompletionContext,
    ) -> Vec<CompletionItem>;
    pub fn framework_navigation_targets(
        &self,
        db: &dyn Database,
        symbol: &Symbol,
    ) -> Vec<NavigationTarget>;
    pub fn framework_virtual_members(&self, db: &dyn Database, class: ClassId) -> Vec<VirtualMember>;
    pub fn virtual_members_for_class(&self, db: &dyn Database, class: ClassId) -> Vec<VirtualMember>;
    pub fn framework_inlay_hints(&self, db: &dyn Database, file: FileId) -> Vec<InlayHint>;
}
```

---

## Spring Framework Support

### Bean Model

```
┌─────────────────────────────────────────────────────────────────┐
│                    SPRING BEAN MODEL                             │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  BEAN DISCOVERY                                                 │
│  • @Component, @Service, @Repository, @Controller               │
│  • @Bean methods in @Configuration classes                      │
│  • XML configuration (legacy)                                   │
│  • @Import and @ComponentScan                                   │
│                                                                  │
│  BEAN METADATA                                                  │
│  • Bean name (explicit or derived)                              │
│  • Type and qualifiers                                          │
│  • Scope (@Scope, @Singleton, @Prototype, etc.)                 │
│  • Profile (@Profile)                                           │
│  • Conditional (@ConditionalOnXxx)                              │
│  • Dependencies (constructor args, @Autowired fields)           │
│                                                                  │
│  RESOLUTION CONTEXT                                              │
│  • Active profiles                                              │
│  • Property sources                                             │
│  • Conditional evaluations                                      │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Implementation status in this repo

Spring support ships as a `nova-framework` analyzer (`nova_framework_spring::SpringAnalyzer`) plus
supporting helpers (config parsing/indexing, DI analysis).

In the IDE there are currently two integration paths:

- `crates/nova-ide/src/framework_cache.rs` provides cache-backed Spring diagnostics/completions for
  config files and `@Value("${...}")` placeholders.
- `crates/nova-ide/src/framework_db.rs` provides a `nova_db::Database` → `nova_framework::Database`
  adapter so `AnalyzerRegistry`-based analyzers can run in-process (see
  `FrameworkAnalyzerRegistryProvider` in `crates/nova-ide/src/extensions.rs`).

---

## Lombok Support

### Lombok Processing Model

```
┌─────────────────────────────────────────────────────────────────┐
│                    LOMBOK PROCESSING                             │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  CHALLENGE                                                      │
│  Lombok generates code at compile time via annotation           │
│  processing. IDE must understand generated members without      │
│  actually running the processor.                                │
│                                                                  │
│  APPROACH: Virtual Members                                      │
│  • Parse Lombok annotations                                     │
│  • Compute what would be generated                              │
│  • Add "virtual" members to class symbol table                  │
│  • These virtual members participate in resolution              │
│                                                                  │
│  SUPPORTED ANNOTATIONS                                          │
│  • @Getter / @Setter                                            │
│  • @Data, @Value                                                │
│  • @Builder                                                     │
│  • @NoArgsConstructor, @AllArgsConstructor, @RequiredArgsConstructor │
│  • @ToString, @EqualsAndHashCode                                │
│  • @Slf4j, @Log, @Log4j2, etc.                                  │
│  • @With                                                        │
│  • @Delegate                                                    │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Lombok Analyzer

Lombok is the primary consumer of the `nova_framework::FrameworkAnalyzer::virtual_members` hook.

- Implementation: `crates/nova-framework-lombok`
- End-to-end IDE wiring example: `crates/nova-ide/src/lombok_intel.rs`

```rust
use nova_core::ProjectId;
use nova_framework::{Database, FrameworkAnalyzer, VirtualMember, VirtualMethod};
use nova_types::ClassId;

pub struct LombokAnalyzer;

impl FrameworkAnalyzer for LombokAnalyzer {
    fn applies_to(&self, db: &dyn Database, project: ProjectId) -> bool {
        db.has_dependency(project, "org.projectlombok", "lombok")
            || db.has_class_on_classpath_prefix(project, "lombok.")
            || db.has_class_on_classpath_prefix(project, "lombok/")
    }

    fn virtual_members(&self, db: &dyn Database, class: ClassId) -> Vec<VirtualMember> {
        let class_data = db.class(class);

        if !class_data.has_annotation("Getter") {
            return Vec::new();
        }

        class_data
            .fields
            .iter()
            .filter(|f| !f.is_static)
            .map(|f| {
                VirtualMember::Method(VirtualMethod {
                    name: format!("get{}", f.name),
                    return_type: f.ty.clone(),
                    params: Vec::new(),
                    is_static: false,
                    span: class_data.annotation_span("Getter"),
                })
            })
            .collect()
    }
}
```

---

## Jakarta EE / JPA Support

JPA support is implemented in `crates/nova-framework-jpa` and exposed as a `nova-framework` analyzer
(`nova_framework_jpa::JpaAnalyzer`) for diagnostics, JPQL completions inside query strings, and
per-file `FrameworkData`.

The underlying model is best-effort and mostly text-based: it parses Java sources with
`nova-syntax`, extracts `@Entity`/relationships, and inspects JPQL strings in annotations.

---

## Annotation Processing Simulation

```
┌─────────────────────────────────────────────────────────────────┐
│            ANNOTATION PROCESSING IN THE IDE                      │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  CHALLENGES                                                     │
│  • Annotation processors generate source code at compile time   │
│  • IDE needs to understand generated code without compiling     │
│  • Must handle: MapStruct, Dagger, AutoValue, Immutables, etc.  │
│                                                                  │
│  STRATEGIES                                                     │
│                                                                  │
│  1. DEDICATED ANALYZERS (Lombok approach)                       │
│     • Hand-coded simulation of specific processors              │
│     • Most accurate for supported processors                    │
│     • Requires maintenance per processor                        │
│                                                                  │
│  2. GENERATED SOURCE DIRECTORIES                                │
│     • Run processors once, index generated sources              │
│     • Works with any processor                                  │
│     • May be stale until rebuild                                │
│                                                                  │
│  3. INCREMENTAL PROCESSOR INVOCATION                            │
│     • Run processors on demand for specific files               │
│     • Most accurate, but slow                                   │
│     • Best for expensive processors                             │
│                                                                  │
│  RECOMMENDATION: Hybrid approach                                │
│  • Dedicated analyzers for common processors                    │
│  • Generated source indexing for others                         │
│  • Optional on-demand processing for validation                 │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

---

## Integration reality (today)

The framework-analyzer API (`nova_framework::Database`/`FrameworkAnalyzer`) is intentionally *not*
the same as Nova’s “file-text database” API (`nova_db::Database`):

- `nova_db::Database` is centered on `file_content(file_id) -> &str` and optional file-path
  enumeration.
- `nova_framework::Database` is centered on `class(ClassId) -> &ClassData` (a class model suitable
  for virtual-member synthesis) plus project mapping and dependency/classpath checks.

That mismatch means IDE/editor integration typically needs an **adapter**:

- Build or reuse an index of classes (often using `nova-syntax`/HIR).
- Populate a `nova_framework::MemoryDatabase` (or another `nova_framework::Database`
  implementation).
- Run analyzers through `nova_framework::AnalyzerRegistry`.

Concrete example: `crates/nova-ide/src/lombok_intel.rs` builds a best-effort workspace index,
feeds it into `MemoryDatabase`, and uses the Lombok analyzer to provide framework-backed member
completion/navigation.

Also note that optional `nova_framework::Database` methods may not be implemented by a host DB:

- `file_text` may return `None`
- `all_files`/`all_classes` may return empty vectors

Analyzers are expected to degrade gracefully by skipping file-text parsing and cross-file scanning
when that information is unavailable.

```rust
use nova_framework::{AnalyzerRegistry, MemoryDatabase};
use nova_framework_lombok::LombokAnalyzer;
use nova_hir::framework::ClassData;

let mut db = MemoryDatabase::new();
let project = db.add_project();
db.add_dependency(project, "org.projectlombok", "lombok");

let class = db.add_class(project, ClassData::default());

let mut registry = AnalyzerRegistry::new();
registry.register(Box::new(LombokAnalyzer::new()));

let _virtuals = registry.framework_virtual_members(&db, class);
```

### Key data types

The API is intentionally small:

```rust
pub struct CompletionContext {
    pub project: ProjectId,
    pub file: FileId,
    pub offset: usize,
}

pub enum Symbol {
    File(FileId),
    Class(ClassId),
}

pub struct NavigationTarget {
    pub file: FileId,
    pub span: Option<Span>,
    pub label: String,
}

pub struct InlayHint {
    pub span: Option<Span>,
    pub label: String,
}
```

### Registry

Analyzers are registered into an `AnalyzerRegistry` (type alias: `FrameworkRegistry`):

```rust
let mut registry = AnalyzerRegistry::new();
registry.register(Box::new(nova_framework_lombok::LombokAnalyzer::new()));
```

In the IDE, the default registry is constructed in `crates/nova-ide/src/extensions.rs` and executed
via `FrameworkAnalyzerRegistryProvider` (which uses `crates/nova-ide/src/framework_db.rs` to adapt
`nova_db::Database` to `nova_framework::Database`).

### Plugin integration constraint (Database adapter)

`FrameworkAnalyzer` runs on `nova_framework::Database`, which requires HIR-backed structural queries
such as `class(ClassId) -> &nova_hir::framework::ClassData`. The IDE-facing `nova_db::Database` is
file-text only.

To use `nova-framework` analyzers in the IDE today, build an adapter (often via
`crates/nova-ide/src/framework_db.rs` or a purpose-built `nova_framework::MemoryDatabase`). See
`crates/nova-ide/src/lombok_intel.rs` for a focused example.

---

## Next Steps

1. → [Performance Engineering](10-performance-engineering.md): Making it all fast
2. → [Editor Integration](11-editor-integration.md): LSP and beyond

---

[← Previous: Refactoring Engine](08-refactoring-engine.md) | [Next: Performance Engineering →](10-performance-engineering.md)
