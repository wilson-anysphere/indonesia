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
- `nova-framework-builtins`: helper crate that centralizes construction of Nova’s built-in
  `nova-framework-*` analyzers (feature-gated for heavier frameworks like Spring/JPA).

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
    CompletionContext, Database, FrameworkData, InlayHint, NavigationTarget, Symbol, VirtualMember,
};
use nova_scheduler::CancellationToken;
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

    fn diagnostics_with_cancel(
        &self,
        db: &dyn Database,
        file: FileId,
        cancel: &CancellationToken,
    ) -> Vec<Diagnostic> {
        if cancel.is_cancelled() {
            Vec::new()
        } else {
            self.diagnostics(db, file)
        }
    }

    fn completions(&self, _db: &dyn Database, _ctx: &CompletionContext) -> Vec<CompletionItem> {
        Vec::new()
    }

    fn completions_with_cancel(
        &self,
        db: &dyn Database,
        ctx: &CompletionContext,
        cancel: &CancellationToken,
    ) -> Vec<CompletionItem> {
        if cancel.is_cancelled() {
            Vec::new()
        } else {
            self.completions(db, ctx)
        }
    }

    fn navigation(&self, _db: &dyn Database, _symbol: &Symbol) -> Vec<NavigationTarget> {
        Vec::new()
    }

    fn navigation_with_cancel(
        &self,
        db: &dyn Database,
        symbol: &Symbol,
        cancel: &CancellationToken,
    ) -> Vec<NavigationTarget> {
        if cancel.is_cancelled() {
            Vec::new()
        } else {
            self.navigation(db, symbol)
        }
    }

    fn virtual_members(&self, _db: &dyn Database, _class: ClassId) -> Vec<VirtualMember> {
        Vec::new()
    }

    fn inlay_hints(&self, _db: &dyn Database, _file: FileId) -> Vec<InlayHint> {
        Vec::new()
    }

    fn inlay_hints_with_cancel(
        &self,
        db: &dyn Database,
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

### `AnalyzerRegistry` aggregation methods

```rust
use nova_framework::{
    AnalyzerRegistry, CompletionContext, Database, FrameworkData, InlayHint, NavigationTarget,
    Symbol, VirtualMember,
};
use nova_scheduler::CancellationToken;
use nova_types::{ClassId, CompletionItem, Diagnostic};
use nova_vfs::FileId;

impl AnalyzerRegistry {
    pub fn framework_data(&self, db: &dyn Database, file: FileId) -> Vec<FrameworkData>;
    pub fn framework_diagnostics(&self, db: &dyn Database, file: FileId) -> Vec<Diagnostic>;
    pub fn framework_diagnostics_with_cancel(
        &self,
        db: &dyn Database,
        file: FileId,
        cancel: &CancellationToken,
    ) -> Vec<Diagnostic>;
    pub fn framework_completions(
        &self,
        db: &dyn Database,
        ctx: &CompletionContext,
    ) -> Vec<CompletionItem>;
    pub fn framework_completions_with_cancel(
        &self,
        db: &dyn Database,
        ctx: &CompletionContext,
        cancel: &CancellationToken,
    ) -> Vec<CompletionItem>;
    pub fn framework_navigation_targets(
        &self,
        db: &dyn Database,
        symbol: &Symbol,
    ) -> Vec<NavigationTarget>;
    pub fn framework_navigation_targets_with_cancel(
        &self,
        db: &dyn Database,
        symbol: &Symbol,
        cancel: &CancellationToken,
    ) -> Vec<NavigationTarget>;
    pub fn framework_virtual_members(&self, db: &dyn Database, class: ClassId) -> Vec<VirtualMember>;
    pub fn virtual_members_for_class(&self, db: &dyn Database, class: ClassId) -> Vec<VirtualMember>;
    pub fn framework_inlay_hints(&self, db: &dyn Database, file: FileId) -> Vec<InlayHint>;
    pub fn framework_inlay_hints_with_cancel(
        &self,
        db: &dyn Database,
        file: FileId,
        cancel: &CancellationToken,
    ) -> Vec<InlayHint>;
}
```

---

## Cache fingerprints and invalidation

Framework analyzers and workspace indexes are frequently queried on every keystroke. Hashing entire
workspaces (full file contents) on each request would be prohibitively expensive, so Nova uses
**cheap, best-effort fingerprints** to key in-memory caches.

### Standard text fingerprint strategy (in-memory buffers)

When a host database provides file text (e.g. `Database::file_text` / `nova_db::Database::file_content`),
Nova fingerprints text as:

1. **Fast identity hash**: `len + as_ptr()`
   - Many Nova DB implementations replace the underlying `String` on edits.
   - Hashing the pointer is therefore a cheap signal that content changed without scanning the
     entire file.
2. **Deterministic content sample** to avoid stale cache hits when:
   - a host mutates text in place (pointer/len stable), or
   - short-lived test fixtures reuse allocations (pointer/len collisions).

Sampling rule (used across `nova-ide` and `nova-framework-*` caches):

- `SAMPLE = 64`
- If `bytes.len() <= 3 * SAMPLE` (≤ 192): hash **all bytes**
- Else hash: `64B prefix + centered 64B middle + 64B suffix`

Pseudo-code:

```rust
use std::hash::{Hash, Hasher};

fn fingerprint_text(text: &str, hasher: &mut impl Hasher) {
    const SAMPLE: usize = 64;
    const FULL_HASH_MAX: usize = 3 * SAMPLE;

    let bytes = text.as_bytes();
    bytes.len().hash(hasher);
    text.as_ptr().hash(hasher);

    if bytes.len() <= FULL_HASH_MAX {
        bytes.hash(hasher);
    } else {
        bytes[..SAMPLE].hash(hasher);
        let mid = bytes.len() / 2;
        let mid_start = mid.saturating_sub(SAMPLE / 2);
        let mid_end = (mid_start + SAMPLE).min(bytes.len());
        bytes[mid_start..mid_end].hash(hasher);
        bytes[bytes.len() - SAMPLE..].hash(hasher);
    }
}
```

### Fallback strategy (unopened buffers / filesystem only)

When the host DB cannot provide file text, caches typically fall back to on-disk metadata:

- file size
- modified time (`mtime`)

This is best-effort; analyzers should treat missing file text as “information not available” and
degrade gracefully.

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
  `crates/nova-ide/src/extensions.rs` for wiring options:
  - `FrameworkAnalyzerRegistryProvider` runs an entire `AnalyzerRegistry` behind one `nova-ext`
    provider ID.
  - `FrameworkAnalyzerAdapterOnTextDb` exposes a single `FrameworkAnalyzer` as its own `nova-ext`
    provider (this is the approach used by `IdeExtensions::with_default_registry` for built-in
    analyzers, allowing per-analyzer isolation and configuration).

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

Also note that some `nova_framework::Database` methods may not be implemented by a host DB:

- `file_text`/`file_path` may return `None`
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

In the IDE, `crates/nova-ide/src/extensions.rs` wires two framework paths:

- The shipped framework diagnostics/completions are currently provided by the legacy
  `FrameworkDiagnosticProvider`/`FrameworkCompletionProvider` (which delegate to
  `crates/nova-ide/src/framework_cache.rs`).
- Built-in `nova-framework` analyzers (Lombok/Dagger/MapStruct/etc) are wired as **per-analyzer**
  providers via `FrameworkAnalyzerAdapterOnTextDb` (one provider per `FrameworkAnalyzer`, see
  `nova_framework_builtins::builtin_analyzers_with_ids()`).

  In `IdeExtensions::with_default_registry`, these providers are configured with
  `with_build_metadata_only()`, meaning they return empty results for “simple” projects (no
  Maven/Gradle/Bazel metadata) to avoid duplicating results from the legacy `framework_cache`
  providers.

  As an alternative integration, `FrameworkAnalyzerRegistryProvider` can run an entire
  `nova_framework::AnalyzerRegistry` behind a single `nova-ext` provider ID (and
  `FrameworkAnalyzerRegistryProvider::empty()` exists as a fast no-op placeholder).

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
