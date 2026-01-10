# Project Nova: Building a Superior Java Language Server

## Executive Summary

This document outlines a comprehensive plan to build **Nova**, a next-generation Java Language Server Protocol (LSP) implementation that aims to surpass the capabilities of IntelliJ IDEA and other existing Java development tools. This is not an incremental improvement—it's a fundamental rethinking of what a Java language server can be.

**Why "Nova"?** A nova is a stellar explosion that dramatically increases a star's brightness. We aim to bring that same transformative brilliance to Java development tooling.

## The Opportunity

IntelliJ IDEA has dominated Java tooling for over two decades. Its excellence comes from deep technical investments that competitors have struggled to match. However, several factors create an unprecedented opportunity:

1. **Architecture Constraints**: IntelliJ was designed as a monolithic IDE, not a composable language server. This creates friction in modern polyglot, editor-agnostic development workflows.

2. **Technical Debt**: 20+ years of evolution means legacy architectural decisions that can't easily be changed.

3. **Modern Techniques**: Advances in incremental computation, query-based compilers, and modern language design (Rust, etc.) provide architectural patterns unavailable when IntelliJ was designed.

4. **AI Revolution**: The integration of AI into development tools requires architectural foundations that weren't anticipated.

5. **Cloud/Remote Development**: Modern development increasingly happens in cloud environments where resource efficiency matters more than ever.

## Document Structure
 
This plan is organized into detailed linked documents covering every aspect of the project:

### Architectural decisions (ADRs)

Binding architectural choices (libraries, core patterns, invariants) are tracked as **Architecture Decision Records**:

- [Architecture + ADR index](docs/architecture.md)

### Part I: Understanding the Problem Space
- **[01 - Problem Analysis: What Makes IntelliJ Excellent](docs/01-problem-analysis.md)** - Deep technical analysis of IntelliJ's architecture and what makes it superior
- **[02 - Current Landscape Analysis](docs/02-current-landscape.md)** - Analysis of existing Java LSPs (Eclipse JDT-LS, etc.) and their limitations
 
### Part II: Architectural Foundation
- **[03 - Architecture Overview](docs/03-architecture-overview.md)** - High-level system architecture and design principles
- **[04 - Incremental Computation Engine](docs/04-incremental-computation.md)** - Query-based incremental computation system (the core innovation)
- **[05 - Syntax and Parsing](docs/05-syntax-and-parsing.md)** - Error-resilient parsing and syntax tree architecture
- **[06 - Semantic Analysis](docs/06-semantic-analysis.md)** - Type checking, symbol resolution, type inference
- **[16 - Java Language Levels and Feature Gating](docs/16-java-language-levels.md)** - Per-module Java version model, preview features, and version-aware diagnostics

### Part III: Intelligence Features
- **[07 - Code Intelligence](docs/07-code-intelligence.md)** - Completions, diagnostics, navigation, code actions
- **[08 - Refactoring Engine](docs/08-refactoring-engine.md)** - Safe, semantic-aware code transformations
- **[09 - Framework Support](docs/09-framework-support.md)** - Spring, Jakarta EE, annotation processing, Lombok

### Part IV: Performance & Integration
- **[10 - Performance Engineering](docs/10-performance-engineering.md)** - Indexing, caching, persistence, concurrency
- **[11 - Editor Integration](docs/11-editor-integration.md)** - LSP protocol, custom extensions, multi-editor support
- **[12 - Debugging Integration](docs/12-debugging-integration.md)** - DAP implementation, advanced debugging features

### Part V: Advanced Capabilities
- **[13 - AI Augmentation](docs/13-ai-augmentation.md)** - Machine learning integration for intelligent features
- **[14 - Testing Strategy](docs/14-testing-strategy.md)** - Comprehensive testing and quality assurance

### Part VI: Project Organization
- **[15 - Work Breakdown](docs/15-work-breakdown.md)** - Suggested organization and phasing

---

## Core Design Principles

### 1. Query-Based Architecture (The Key Innovation)

Unlike traditional compilers that process files sequentially, Nova uses a **query-based incremental computation engine** inspired by systems like Salsa (rust-analyzer) and Adapton. Every piece of information in the system is the result of a query, and queries automatically track their dependencies.

```
┌─────────────────────────────────────────────────────────────────┐
│                    Query Database                                │
├─────────────────────────────────────────────────────────────────┤
│  Input Queries      │  Derived Queries                          │
│  ─────────────────  │  ────────────────────────────────────────  │
│  • file_content     │  • parse_file → Syntax Tree               │
│  • file_exists      │  • resolve_imports → Import Resolution    │
│  • config           │  • type_check → Type Information          │
│                     │  • completions_at → Completion Items      │
│                     │  • diagnostics_for → Error Messages       │
└─────────────────────────────────────────────────────────────────┘
```

**Why this matters:**
- Automatic incremental updates when files change
- Only recompute what's affected
- Parallel execution of independent queries
- Deterministic behavior
- Trivial persistence and caching

### 2. Resilient by Design

Unlike a batch compiler, a language server must work with broken, incomplete code. Nova is designed from the ground up to:

- **Parse broken syntax** and recover meaningfully
- **Type-check partial programs** with graceful degradation
- **Provide useful information** even when errors exist
- **Never crash or hang** regardless of input

### 3. Performance as a Feature

Performance isn't just optimization—it's a core design constraint. Every architectural decision must consider:

- **Latency**: Keystroke-level responsiveness (< 16ms for most operations)
- **Memory**: Efficient handling of large codebases (millions of lines)
- **Startup**: Fast time-to-useful (< 2 seconds for basic features)
- **Scaling**: Linear or better scaling with codebase size

### 4. Composability Over Integration

Instead of building a monolithic system, Nova provides composable building blocks:

- **Standalone semantic database**: Can be used by any tool
- **Pluggable framework analyzers**: Add framework support without core changes
- **Standard protocols**: LSP, DAP, custom extensions
- **Library-first design**: Every component usable independently

---

## Technical Innovation Highlights

### Innovation 1: Demand-Driven Type Checking

Traditional Java type checkers process entire compilation units. Nova introduces **demand-driven type checking** that computes type information on-demand:

- Open a file? Only check that file (and dependencies as needed)
- Hover over an expression? Only compute that expression's type
- No wasted work on unused code paths

### Innovation 2: Hybrid Persistence Model

Nova maintains a persistent, memory-mapped database that survives restarts:

- First keystroke to useful: Sub-second (indexes already loaded)
- Background indexing: Continues where it left off after crashes
- Shared indexes: Teams can share prebuilt indexes for common dependencies

### Innovation 3: Semantic Diff Engine

Instead of treating refactoring as text manipulation, Nova works with **semantic diffs**:

- Refactoring produces semantic changes, not text edits
- Changes can be previewed, modified, and composed
- Multi-file refactorings maintain consistency guarantees

### Innovation 4: Framework-Aware Analysis

Deep, first-class support for frameworks like Spring:

- Understands dependency injection at the semantic level
- Bean resolution, autowiring, configuration validation
- Navigation between code and configuration
- Detects misconfigurations before runtime

### Innovation 5: AI-Native Architecture

Built with AI integration as a first-class concern:

- Embedding generation for semantic code search
- Context assembly for LLM prompts
- Structured output parsing for AI-generated code
- Hybrid analysis combining static and AI-based reasoning

---

## Success Metrics

Nova will be considered successful when it demonstrably exceeds IntelliJ on these dimensions:

| Metric | IntelliJ Baseline | Nova Target |
|--------|-------------------|-------------|
| Completion latency (p95) | ~100ms | <50ms |
| Rename refactoring (1000 usages) | ~2s | <500ms |
| Memory (1M LOC project) | ~4GB | <1.5GB |
| Time to first completion | ~10s | <2s |
| Framework support depth | Best-in-class | Match or exceed |
| Recovery from syntax errors | Good | Excellent |

---

## Critical Challenges

### Challenge 1: Java's Complexity

Java is a complex language with:
- Generics with complex variance and bounds
- Type inference (var, diamond, lambda parameters)
- Annotation processing that generates code
- Module system (JPMS)
- Multiple inheritance of interface methods
- Sealed types, records, pattern matching

**Mitigation**: Build on proven type system implementations, extensive test suite against Java specification.

### Challenge 2: Framework Magic

Modern Java development relies heavily on frameworks (Spring, Jakarta EE) that use:
- Runtime reflection
- Annotation-driven code generation
- Convention over configuration
- Proxy-based AOP

**Mitigation**: Dedicated framework analyzers, configurable analysis scope, hybrid static/dynamic analysis.

### Challenge 3: Scale

Large enterprise codebases can have:
- Millions of lines of code
- Thousands of modules
- Deep dependency trees
- Complex build configurations

**Mitigation**: Incremental architecture, persistent indexes, distributed analysis capability.

### Challenge 4: Ecosystem Compatibility

Must work with:
- Multiple build tools (Maven, Gradle, Bazel)
- Various Java versions (8, 11, 17, 21+)
- Existing developer workflows
- Team conventions and configurations

**Mitigation**: Plugin architecture for build tools, version-aware analysis, configuration layering.

---

## Next Steps

This document provides the strategic overview. For detailed technical design, implementation guidance, and work breakdown, proceed to the linked documents.

**Recommended reading order for technical depth:**
1. [Problem Analysis](docs/01-problem-analysis.md) - Understand what we're competing against
2. [Architecture Overview](docs/03-architecture-overview.md) - Core system design
3. [Incremental Computation](docs/04-incremental-computation.md) - The key innovation
4. [Semantic Analysis](docs/06-semantic-analysis.md) - The hardest technical problem

**For project planning:**
1. [Work Breakdown](docs/15-work-breakdown.md) - Organization and phasing
2. [Testing Strategy](docs/14-testing-strategy.md) - Quality assurance approach

---

## A Note on Ambition

Building a Java language server superior to IntelliJ is one of the most ambitious projects in developer tooling. IntelliJ represents 20+ years of investment by hundreds of engineers. This is not a weekend project.

However, several factors make success possible:
1. **Modern architectural patterns** that weren't available when IntelliJ was designed
2. **Lessons learned** from successful projects like rust-analyzer
3. **Focus**: We're building a language server, not a full IDE
4. **Collaboration**: Unlimited intelligent agents working in concert

The path is difficult but not impossible. This document and its companions provide the map.

---

*Document Version: 1.0*  
*Created: January 2026*
