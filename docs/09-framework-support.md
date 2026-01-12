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

### Spring Analyzer Implementation

Spring support ships as a `nova-framework` analyzer (`nova_framework_spring::SpringAnalyzer`) plus
supporting helpers (config parsing/indexing, DI analysis).

In the IDE, analyzers are executed via a `nova_db::Database` → `nova_framework::Database` adapter
(`crates/nova-ide/src/framework_db.rs`) and the default `AnalyzerRegistry` registered by `nova-ide`
(see `crates/nova-ide/src/extensions.rs`). See "Plugin integration constraint" below.

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
pub struct LombokAnalyzer;

impl FrameworkAnalyzer for LombokAnalyzer {
    fn applies_to(&self, db: &dyn Database, project: ProjectId) -> bool {
        db.has_dependency(project, "org.projectlombok", "lombok")
    }

    fn virtual_members(&self, db: &dyn Database, class: ClassId) -> Vec<VirtualMember> {
        let class_data = db.class(class);
        let _ = class_data;
        // Produce `VirtualMember::{Field,Method,Constructor,InnerClass}` values.
        Vec::new()
    }
}
```

---

## Jakarta EE / JPA Support

JPA support is implemented in `crates/nova-framework-jpa` and exposed as a `nova-framework` analyzer
(`nova_framework_jpa::JpaAnalyzer`) for diagnostics and JPQL completions inside query strings. The
underlying model is best-effort and mostly text-based.

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

## Framework Plugin System

The `nova-framework` crate provides a minimal plugin API used by resolution and (optionally) the
IDE.

### `FrameworkAnalyzer`

`applies_to` is required; all other hooks are optional and default to no-ops.

```rust
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
