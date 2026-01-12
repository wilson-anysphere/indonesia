# 02 - Current Landscape Analysis

[← Back to Main Document](../AGENTS.md) | [Previous: Problem Analysis](01-problem-analysis.md)

## Overview

Before building Nova, we must understand the current state of Java language servers. This analysis covers existing implementations, their architectures, strengths, limitations, and lessons to learn.

---

## Existing Java Language Servers

### Eclipse JDT Language Server (jdt.ls)

**Repository**: github.com/eclipse-jdtls/eclipse.jdt.ls

The most mature Java LSP implementation, used by VS Code's Java extension.

```
┌─────────────────────────────────────────────────────────────────┐
│                    JDT.LS Architecture                           │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │                 Eclipse JDT Core                         │    │
│  │  • Java parser/compiler (ecj)                           │    │
│  │  • AST and DOM model                                    │    │
│  │  • Type binding resolution                              │    │
│  │  • Index system                                         │    │
│  └─────────────────────────────────────────────────────────┘    │
│                              │                                   │
│                              ▼                                   │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │                   JDT.LS Layer                           │    │
│  │  • LSP protocol implementation                          │    │
│  │  • Request handlers                                     │    │
│  │  • Build tool integration (Maven, Gradle)               │    │
│  │  • Workspace management                                 │    │
│  └─────────────────────────────────────────────────────────┘    │
│                              │                                   │
│                              ▼                                   │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │                   LSP Protocol                           │    │
│  │  • JSON-RPC over stdio/socket                           │    │
│  │  • Standard LSP messages                                │    │
│  └─────────────────────────────────────────────────────────┘    │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

**Strengths:**
- Mature, well-tested Eclipse JDT compiler core
- Comprehensive Java language support
- Good Maven/Gradle integration
- Active development and community
- Extensive real-world usage

**Limitations:**
- Inherited Eclipse architecture (batch-oriented)
- Memory-heavy (often 1-2GB for medium projects)
- Slow startup (index building)
- Limited incremental updates
- Framework support lacking compared to IntelliJ
- Error recovery could be better

**Performance Characteristics:**
| Metric | Typical Value |
|--------|---------------|
| Startup time | 15-45 seconds |
| Memory (medium project) | 1-2 GB |
| Completion latency | 100-500ms |
| Rename refactoring | 1-5 seconds |

### IntelliJ-Based LSP

JetBrains has added LSP server capabilities to IntelliJ Platform.

**Approach:**
- Full IntelliJ Platform running headlessly
- PSI and all IntelliJ features available
- Exposed through LSP protocol

**Strengths:**
- Full IntelliJ feature parity (eventually)
- Best-in-class analysis

**Limitations:**
- Requires full IntelliJ installation
- Very resource-intensive
- Not truly lightweight
- Commercial licensing complications

### Metals (for Scala, instructive comparison)

While not Java-specific, Metals shows modern LSP design:

**Innovations:**
- Build server protocol (BSP) separation
- Incremental compilation via Bloop
- SemanticDB for semantic information
- Focused on being an LSP, not an IDE

**Lessons for Nova:**
- Separate concerns (build vs. analysis)
- Use existing compilers where beneficial
- Design for LSP from the start

---

## LSP Protocol Analysis

### What LSP Provides

```
┌─────────────────────────────────────────────────────────────────┐
│                    LSP Capabilities                              │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  TEXT SYNCHRONIZATION                                            │
│  • Full document sync                                           │
│  • Incremental sync                                             │
│  • Save notifications                                           │
│                                                                  │
│  LANGUAGE FEATURES                                               │
│  • Hover (documentation)                                        │
│  • Completion (+ completion item resolve)                       │
│  • Signature help                                               │
│  • Go to definition/declaration/type definition                 │
│  • Find references                                              │
│  • Document highlight                                           │
│  • Document symbols                                             │
│  • Workspace symbols                                            │
│  • Code actions (quick fixes, refactoring)                      │
│  • Code lens                                                    │
│  • Document formatting                                          │
│  • Rename                                                       │
│  • Folding range                                                │
│  • Selection range                                              │
│  • Semantic tokens                                              │
│  • Inlay hints                                                  │
│  • Call hierarchy                                               │
│  • Type hierarchy                                               │
│                                                                  │
│  WORKSPACE FEATURES                                              │
│  • Workspace edits                                              │
│  • File operations                                              │
│  • Configuration                                                │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### LSP Limitations for Java

The LSP protocol, while comprehensive, has limitations for Java development:

1. **No Build Integration**: LSP doesn't define how to integrate with build systems
2. **Limited Refactoring Model**: Code actions are simple; complex refactoring UIs need extensions
3. **No Debugging**: DAP is separate protocol
4. **No Test Discovery**: Testing support requires extensions
5. **Limited Project Model**: Workspace folders are simple; module systems need more

### Required LSP Extensions for Nova
 
Nova will need to extend LSP for Java-specific needs:
 
```
┌─────────────────────────────────────────────────────────────────┐
│                    Nova LSP Extensions                           │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  PROJECT EXTENSIONS                                              │
│  • nova/projectConfiguration - project structure info           │
│  • nova/java/classpath - classpath info                         │
│  • nova/java/sourcePaths - source root changes                  │
│  • nova/reloadProject - force project reload                    │
│                                                                  │
│  BUILD EXTENSIONS                                                │
│  • nova/buildProject - trigger build                            │
│  • nova/build/status - poll build status                        │
│  • nova/build/diagnostics - build errors/diagnostics            │
│                                                                  │
│  REFACTORING EXTENSIONS                                          │
│  • nova/refactor/preview - preview refactoring changes          │
│  • nova/refactor/apply - apply refactoring with options         │
│                                                                  │
│  FRAMEWORK EXTENSIONS                                            │
│  • nova/web/endpoints - HTTP endpoint discovery                 │
│  • nova/quarkus/endpoints - HTTP endpoint discovery (alias)     │
│  • nova/micronaut/endpoints - Micronaut endpoint discovery      │
│  • nova/micronaut/beans - Micronaut bean information            │
│                                                                  │
│  DEBUGGING EXTENSIONS                                            │
│  • nova/debug/configurations - debug launch configs             │
│  • nova/debug/hotSwap - hot code replacement                    │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

Note: the **source of truth** for custom `nova/*` method names and JSON schemas is
[`docs/protocol-extensions.md`](protocol-extensions.md).

---

## Analysis of Java Language Complexity

### Java Versioning Challenge
 
Nova must support multiple Java versions with their varying features:

```
┌─────────────────────────────────────────────────────────────────┐
│                    Java Feature Timeline                         │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  Java 8 (2014) - LTS, still heavily used                        │
│  • Lambdas, method references                                   │
│  • Default methods                                              │
│  • Stream API                                                   │
│                                                                  │
│  Java 9 (2017)                                                  │
│  • Module system (JPMS)                                         │
│  • Private interface methods                                    │
│                                                                  │
│  Java 10 (2018)                                                 │
│  • Local variable type inference (var)                          │
│                                                                  │
│  Java 11 (2018) - LTS                                           │
│  • var in lambdas                                               │
│                                                                  │
│  Java 14 (2020)                                                 │
│  • Records (preview)                                            │
│  • Pattern matching instanceof (preview)                        │
│                                                                  │
│  Java 15 (2020)                                                 │
│  • Sealed classes (preview)                                     │
│  • Text blocks                                                  │
│                                                                  │
│  Java 16 (2021)                                                 │
│  • Records (final)                                              │
│  • Pattern matching instanceof (final)                          │
│                                                                  │
│  Java 17 (2021) - LTS                                           │
│  • Sealed classes (final)                                       │
│                                                                  │
│  Java 21 (2023) - LTS                                           │
│  • Virtual threads                                              │
│  • Record patterns                                              │
│  • Pattern matching for switch (final)                          │
│  • Sequenced collections                                        │
│                                                                  │
│  Java 22+ (2024+)                                               │
│  • Unnamed variables                                            │
│  • String templates (preview)                                   │
│  • Primitive patterns (preview)                                 │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```
 
The hard part is not just "parsing the newest syntax"—it's doing **version-aware analysis**:
- language level is **per module** (Maven module / Gradle subproject / Bazel target)
- preview features must be explicitly enabled (`--enable-preview`)
- the same source can be legal or illegal depending on that configuration

Nova should model this explicitly as a single `JavaLanguageLevel` value that is threaded through parsing, semantics, and diagnostics. See:
- [16 - Java Language Levels and Feature Gating](16-java-language-levels.md)
 
### Type System Complexity

Java's type system has accumulated significant complexity:

```
┌─────────────────────────────────────────────────────────────────┐
│                    Type System Challenges                        │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  GENERICS                                                        │
│  • Parameterized types: List<String>                            │
│  • Bounded wildcards: List<? extends Number>                    │
│  • Type variable bounds: <T extends Comparable<T>>              │
│  • Raw types (legacy compatibility)                             │
│  • Type erasure implications                                    │
│                                                                  │
│  TYPE INFERENCE                                                  │
│  • Diamond operator: new ArrayList<>()                          │
│  • Lambda parameter types                                       │
│  • var declarations                                             │
│  • Generic method invocation                                    │
│  • Target typing for lambdas and method refs                    │
│                                                                  │
│  INTERSECTION TYPES                                              │
│  • <T extends A & B> - multiple bounds                          │
│  • Cast intersection types: (Serializable & Comparable)         │
│                                                                  │
│  SPECIAL CASES                                                   │
│  • Bridge methods                                               │
│  • Covariant return types                                       │
│  • Capture conversion                                           │
│  • Type inference with overloading                              │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Build Tool Complexity

Java projects use various build tools with different models:

```
┌─────────────────────────────────────────────────────────────────┐
│                    Build Tool Landscape                          │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  MAVEN                                                           │
│  • XML-based (pom.xml)                                          │
│  • Convention over configuration                                │
│  • Dependency management with repositories                      │
│  • Multi-module projects                                        │
│  • Lifecycle phases                                             │
│  • Plugin system                                                │
│                                                                  │
│  GRADLE                                                          │
│  • Groovy or Kotlin DSL                                         │
│  • Highly customizable                                          │
│  • Incremental builds                                           │
│  • Build cache                                                  │
│  • Multi-project builds                                         │
│  • Tooling API for IDE integration                              │
│                                                                  │
│  BAZEL / BUCK                                                    │
│  • Hermetic builds                                              │
│  • BUILD files                                                  │
│  • Remote caching and execution                                 │
│  • Language-agnostic                                            │
│                                                                  │
│  ANT (legacy)                                                    │
│  • XML-based                                                    │
│  • Explicit task definitions                                    │
│  • Still found in older codebases                               │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

---

## Lessons from Other Language Servers

### rust-analyzer (Rust)

The gold standard for modern language server design.

**Key Innovations:**
- Query-based architecture (Salsa)
- Demand-driven analysis
- Incremental computation
- Excellent error recovery
- Written from scratch for LSP

**What Nova can learn:**
- Query database pattern
- On-demand computation
- Syntax tree design (rowan)
- Testing strategies

### TypeScript Language Server (tsserver)

Mature, high-performance JavaScript/TypeScript support.

**Key Features:**
- Incremental parsing
- Project references
- Excellent completion
- Good error messages

**What Nova can learn:**
- Project model handling
- Incremental strategies
- Completion ranking

### clangd (C/C++)

C/C++ language server built on Clang.

**Key Features:**
- Built on production compiler
- Background indexing
- Compile commands integration
- Excellent performance

**What Nova can learn:**
- Compiler integration patterns
- Index persistence
- Background processing

### gopls (Go)

The official Go language server.

**Key Features:**
- Module-aware
- Integrated with go command
- Good workspace support

**What Nova can learn:**
- Build system integration
- Module handling

---

## Gap Analysis: What's Missing

### Gaps in Existing Java LSPs

| Capability | JDT.LS | IntelliJ | Gap for Nova |
|------------|--------|----------|--------------|
| Basic completion | Good | Excellent | Match IntelliJ |
| Smart completion | Limited | Excellent | Exceed both |
| Refactoring breadth | Moderate | Excellent | Match IntelliJ |
| Refactoring UX | Poor | Excellent | Exceed both |
| Error recovery | Moderate | Good | Exceed both |
| Framework support | Basic | Excellent | Match IntelliJ |
| Startup time | Slow | Slow | Exceed both |
| Memory usage | High | Very high | Exceed both |
| Incremental updates | Limited | Moderate | Exceed both |

### Opportunities for Innovation

1. **True Incremental Analysis**
   - Neither JDT.LS nor IntelliJ has fully demand-driven analysis
   - Nova can pioneer query-based Java analysis

2. **Resource Efficiency**
   - Both existing options are memory-hungry
   - Nova can target 10x memory reduction

3. **Fast Startup**
   - Both require lengthy initialization
   - Nova can target sub-second startup

4. **Framework Intelligence**
   - IntelliJ leads, but it's still limited
   - Nova can push boundaries with dedicated framework analyzers

5. **AI Integration**
   - Neither is designed for AI integration
   - Nova can be AI-native from the start

6. **Remote Development**
   - Both are optimized for local
   - Nova can be remote-first

---

## Technology Decisions

Based on this analysis, key technology decisions for Nova:

### Language Choice

**Recommendation: Rust**

Rationale:
- Memory safety without GC (predictable latency)
- Excellent concurrency primitives
- rust-analyzer proves viability
- Salsa framework available
- Can compile to WASM for browser use
- Growing ecosystem for developer tools

Alternatives considered:
- **Java/Kotlin**: Familiar to team, but GC pauses problematic
- **Go**: Good performance, but less expressive type system
- **C++**: High performance, but memory safety concerns

### Parser Approach

**Recommendation: Custom hand-written parser**

Rationale:
- Full control over error recovery
- Optimal performance
- Can evolve with Java versions
- Best error messages

Alternatives considered:
- **Tree-sitter**: Fast, but error recovery limited
- **ANTLR**: Mature, but generated code hard to customize
- **ECJ (Eclipse compiler)**: Mature, but batch-oriented

### Persistence Layer

**Recommendation: Memory-mapped custom format + SQLite for metadata**

Rationale:
- Memory-mapped files for fast startup
- SQLite for queryable metadata
- Custom format for performance-critical data
- Incremental updates possible

---

## Competitive Positioning

Nova should position itself as:

```
┌─────────────────────────────────────────────────────────────────┐
│                    Nova Positioning                              │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  vs. JDT.LS:                                                    │
│  "Same or better features, 10x faster, 5x less memory"          │
│                                                                  │
│  vs. IntelliJ:                                                   │
│  "IntelliJ-quality analysis, editor-agnostic, resource-light"   │
│                                                                  │
│  vs. Both:                                                       │
│  "Built for modern development: remote-first, AI-native,        │
│   instant startup"                                              │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

---

## Next Steps

1. → [Architecture Overview](03-architecture-overview.md): Design Nova's architecture based on this analysis
2. → [Incremental Computation](04-incremental-computation.md): Deep dive on query-based architecture

---

[← Previous: Problem Analysis](01-problem-analysis.md) | [Next: Architecture Overview →](03-architecture-overview.md)
