# 15 - Work Breakdown

[← Back to Main Document](../AGENTS.md) | [Previous: Testing Strategy](14-testing-strategy.md)

## Overview

This document suggests how the Nova project might be organized and phased. The goal is to deliver incremental value while building toward the complete vision.

---

## Project Phases

```
┌─────────────────────────────────────────────────────────────────┐
│                    PROJECT PHASES                                │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  PHASE 0: FOUNDATION (Months 1-3)                               │
│  ──────────────────────────────────                             │
│  Goal: Working skeleton with basic features                     │
│  • Core infrastructure                                          │
│  • Basic parsing and type checking                              │
│  • Minimal LSP server                                           │
│  Milestone: Can open Java file, see basic errors                │
│                                                                  │
│  PHASE 1: CORE INTELLIGENCE (Months 4-8)                        │
│  ─────────────────────────────────────────                      │
│  Goal: Competitive with basic IDE features                      │
│  • Full Java parsing with error recovery                        │
│  • Complete type system                                         │
│  • Code completion                                              │
│  • Navigation (go to def, find refs)                            │
│  Milestone: Usable for daily Java development                   │
│                                                                  │
│  PHASE 2: ADVANCED FEATURES (Months 9-14)                       │
│  ──────────────────────────────────────────                     │
│  Goal: Feature parity with mature tools                         │
│  • Refactoring engine                                           │
│  • Framework support (Spring, etc.)                             │
│  • Debug adapter                                                │
│  • Performance optimization                                     │
│  Milestone: Can replace existing Java LSPs                      │
│                                                                  │
│  PHASE 3: EXCELLENCE (Months 15-20)                             │
│  ────────────────────────────────────                           │
│  Goal: Surpass IntelliJ on key dimensions                       │
│  • AI integration                                               │
│  • Advanced framework support                                   │
│  • Performance leadership                                       │
│  • Polish and refinement                                        │
│  Milestone: Best-in-class Java development experience           │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

---

## Detailed Work Areas

### Area 1: Core Infrastructure

```
┌─────────────────────────────────────────────────────────────────┐
│                    CORE INFRASTRUCTURE                           │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  QUERY DATABASE                                                 │
│  • Design query system architecture                             │
│  • Implement base query framework                               │
│  • Add dependency tracking                                      │
│  • Implement memoization and caching                           │
│  • Add parallel query execution                                 │
│  • Implement persistence layer                                  │
│                                                                  │
│  FILE SYSTEM                                                    │
│  • Virtual file system abstraction                              │
│  • File watching and change detection                           │
│  • JAR file reading                                             │
│  • Classpath management                                         │
│                                                                  │
│  INDEX SYSTEM                                                   │
│  • Design index structures                                      │
│  • Implement symbol index                                       │
│  • Implement reference index                                    │
│  • Implement inheritance index                                  │
│  • Add persistence and incremental updates                      │
│                                                                  │
│  PROJECT MODEL                                                  │
│  • Maven project support                                        │
│  • Gradle project support                                       │
│  • Multi-module project support                                 │
│  • Configuration management                                     │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Area 2: Syntax Layer

```
┌─────────────────────────────────────────────────────────────────┐
│                    SYNTAX LAYER                                  │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  LEXER                                                          │
│  • Token definitions for Java                                   │
│  • Lexer implementation                                         │
│  • Support all Java versions (8-21+)                            │
│  • Performance optimization                                     │
│                                                                  │
│  PARSER                                                         │
│  • Grammar definition                                           │
│  • Recursive descent parser                                     │
│  • Pratt parsing for expressions                                │
│  • Error recovery strategies                                    │
│  • Incremental parsing                                          │
│                                                                  │
│  SYNTAX TREES                                                   │
│  • Green/red tree implementation                                │
│  • Typed syntax API                                             │
│  • Tree traversal utilities                                     │
│  • Source text reconstruction                                   │
│                                                                  │
│  JAVA FEATURES                                                  │
│  • Records (Java 16+)                                           │
│  • Sealed classes (Java 17+)                                    │
│  • Pattern matching (Java 21+)                                  │
│  • Preview features handling                                    │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Area 3: Semantic Analysis

```
┌─────────────────────────────────────────────────────────────────┐
│                    SEMANTIC ANALYSIS                             │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  NAME RESOLUTION                                                │
│  • Scope model                                                  │
│  • Import resolution                                            │
│  • Package resolution                                           │
│  • Qualified name resolution                                    │
│                                                                  │
│  TYPE SYSTEM                                                    │
│  • Type representation                                          │
│  • Primitive types                                              │
│  • Class/interface types                                        │
│  • Array types                                                  │
│  • Generic types with bounds                                    │
│  • Wildcard types                                               │
│  • Intersection types                                           │
│                                                                  │
│  TYPE CHECKING                                                  │
│  • Subtyping                                                    │
│  • Assignment compatibility                                     │
│  • Method overload resolution                                   │
│  • Type inference (var, diamond, lambda)                        │
│                                                                  │
│  FLOW ANALYSIS                                                  │
│  • Control flow graph                                           │
│  • Definite assignment                                          │
│  • Reachability                                                 │
│  • Null analysis                                                │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Area 4: Code Intelligence

```
┌─────────────────────────────────────────────────────────────────┐
│                    CODE INTELLIGENCE                             │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  COMPLETIONS                                                    │
│  • Context analysis                                             │
│  • Member completion                                            │
│  • Keyword completion                                           │
│  • Import completion                                            │
│  • Postfix completion                                           │
│  • Smart/type-aware completion                                  │
│  • Completion ranking                                           │
│                                                                  │
│  DIAGNOSTICS                                                    │
│  • Error collection                                             │
│  • Warning generation                                           │
│  • Hint generation                                              │
│  • Diagnostic formatting                                        │
│                                                                  │
│  NAVIGATION                                                     │
│  • Go to definition                                             │
│  • Find references                                              │
│  • Find implementations                                         │
│  • Type hierarchy                                               │
│  • Call hierarchy                                               │
│                                                                  │
│  CODE ACTIONS                                                   │
│  • Quick fixes                                                  │
│  • Intention actions                                            │
│  • Source generation                                            │
│                                                                  │
│  OTHER FEATURES                                                 │
│  • Hover information                                            │
│  • Signature help                                               │
│  • Document symbols                                             │
│  • Semantic highlighting                                        │
│  • Inlay hints                                                  │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Area 5: Refactoring

```
┌─────────────────────────────────────────────────────────────────┐
│                    REFACTORING                                   │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  INFRASTRUCTURE                                                 │
│  • Semantic diff model                                          │
│  • Conflict detection                                           │
│  • Preview generation                                           │
│  • Edit application                                             │
│                                                                  │
│  BASIC REFACTORINGS                                             │
│  • Rename (all symbol types)                                    │
│  • Extract variable                                             │
│  • Extract constant                                             │
│  • Inline variable                                              │
│                                                                  │
│  INTERMEDIATE REFACTORINGS                                      │
│  • Extract method                                               │
│  • Inline method                                                │
│  • Change signature                                             │
│  • Move class                                                   │
│                                                                  │
│  ADVANCED REFACTORINGS                                          │
│  • Extract interface                                            │
│  • Pull up / push down                                          │
│  • Introduce parameter object                                   │
│  • Convert to record                                            │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Area 6: Framework Support

```
┌─────────────────────────────────────────────────────────────────┐
│                    FRAMEWORK SUPPORT                             │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  PLUGIN ARCHITECTURE                                            │
│  • Framework analyzer interface                                 │
│  • Registration and discovery                                   │
│  • Lifecycle management                                         │
│                                                                  │
│  LOMBOK                                                         │
│  • @Getter/@Setter                                              │
│  • @Data, @Value                                                │
│  • @Builder                                                     │
│  • @Slf4j and logging                                           │
│  • Constructor annotations                                      │
│                                                                  │
│  SPRING                                                         │
│  • Bean discovery                                               │
│  • Autowiring resolution                                        │
│  • Configuration validation                                     │
│  • Profile support                                              │
│  • Spring Boot support                                          │
│                                                                  │
│  JPA/HIBERNATE                                                  │
│  • Entity analysis                                              │
│  • Relationship validation                                      │
│  • Query language support                                       │
│                                                                  │
│  OTHER FRAMEWORKS                                               │
│  • Jakarta EE                                                   │
│  • Micronaut                                                    │
│  • Quarkus                                                      │
│  • Custom frameworks                                            │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Area 7: Editor Integration

```
┌─────────────────────────────────────────────────────────────────┐
│                    EDITOR INTEGRATION                            │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  LSP SERVER                                                     │
│  • Protocol implementation                                      │
│  • Request handling                                             │
│  • Document synchronization                                     │
│  • Progress reporting                                           │
│  • Custom extensions                                            │
│                                                                  │
│  VS CODE EXTENSION                                              │
│  • Extension scaffolding                                        │
│  • UI integration                                               │
│  • Configuration                                                │
│  • Debug integration                                            │
│                                                                  │
│  OTHER EDITORS                                                  │
│  • Neovim configuration                                         │
│  • Emacs configuration                                          │
│  • Helix configuration                                          │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Area 8: Debug Adapter

```
┌─────────────────────────────────────────────────────────────────┐
│                    DEBUG ADAPTER                                 │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  DAP IMPLEMENTATION                                             │
│  • Protocol implementation                                      │
│  • Launch/attach                                                │
│  • Breakpoints                                                  │
│  • Stepping                                                     │
│  • Variables/evaluation                                         │
│                                                                  │
│  JVM INTEGRATION                                                │
│  • JDWP client                                                  │
│  • Thread management                                            │
│  • Stack frame handling                                         │
│                                                                  │
│  ADVANCED FEATURES                                              │
│  • Hot code replacement                                         │
│  • Smart step into                                              │
│  • Stream debugger                                              │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Area 9: AI Features

```
┌─────────────────────────────────────────────────────────────────┐
│                    AI FEATURES                                   │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  INFRASTRUCTURE                                                 │
│  • Model management                                             │
│  • Context building                                             │
│  • Privacy controls                                             │
│  • Cloud provider integration                                   │
│                                                                  │
│  FEATURES                                                       │
│  • Completion ranking                                           │
│  • Code generation                                              │
│  • Error explanation                                            │
│  • Semantic search                                              │
│  • Test generation                                              │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Area 10: Performance

```
┌─────────────────────────────────────────────────────────────────┐
│                    PERFORMANCE                                   │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  OPTIMIZATION                                                   │
│  • Profiling infrastructure                                     │
│  • Cache tuning                                                 │
│  • Memory optimization                                          │
│  • Parallelization                                              │
│                                                                  │
│  PERSISTENCE                                                    │
│  • Index persistence                                            │
│  • Query cache persistence                                      │
│  • Startup optimization                                         │
│                                                                  │
│  SCALING                                                        │
│  • Large project support                                        │
│  • Distributed analysis                                         │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

---

## Suggested Team Structure

```
┌─────────────────────────────────────────────────────────────────┐
│                    TEAM SUGGESTIONS                              │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  CORE TEAM                                                      │
│  Responsible for foundation and architecture                    │
│  • Query database                                               │
│  • Infrastructure                                               │
│  • Performance                                                  │
│                                                                  │
│  PARSER TEAM                                                    │
│  Responsible for syntax layer                                   │
│  • Lexer and parser                                             │
│  • Error recovery                                               │
│  • Java version support                                         │
│                                                                  │
│  SEMANTICS TEAM                                                 │
│  Responsible for type system                                    │
│  • Name resolution                                              │
│  • Type checking                                                │
│  • Flow analysis                                                │
│                                                                  │
│  INTELLIGENCE TEAM                                              │
│  Responsible for user features                                  │
│  • Completions                                                  │
│  • Navigation                                                   │
│  • Code actions                                                 │
│                                                                  │
│  REFACTORING TEAM                                               │
│  Responsible for code transformations                           │
│  • Refactoring engine                                           │
│  • Individual refactorings                                      │
│                                                                  │
│  FRAMEWORK TEAM                                                 │
│  Responsible for framework support                              │
│  • Spring analyzer                                              │
│  • Lombok analyzer                                              │
│  • Other frameworks                                             │
│                                                                  │
│  INTEGRATION TEAM                                               │
│  Responsible for external interfaces                            │
│  • LSP server                                                   │
│  • Debug adapter                                                │
│  • Editor extensions                                            │
│                                                                  │
│  AI TEAM                                                        │
│  Responsible for ML features                                    │
│  • Model integration                                            │
│  • AI features                                                  │
│                                                                  │
│  QA TEAM                                                        │
│  Responsible for quality                                        │
│  • Test infrastructure                                          │
│  • Specification tests                                          │
│  • Performance testing                                          │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

---

## Dependencies

```
┌─────────────────────────────────────────────────────────────────┐
│                    DEPENDENCY GRAPH                              │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  Query Database ───────────────────────────────────────┐        │
│       │                                                │        │
│       ▼                                                │        │
│  File System ─────────────────────────────────┐       │        │
│       │                                        │       │        │
│       ▼                                        │       │        │
│  Lexer/Parser ──────────────────────┐         │       │        │
│       │                              │         │       │        │
│       ▼                              │         │       │        │
│  Syntax Trees ───────────┐          │         │       │        │
│       │                   │          │         │       │        │
│       ▼                   │          │         │       │        │
│  Name Resolution ─────────┤          │         │       │        │
│       │                   │          │         │       │        │
│       ▼                   │          │         │       │        │
│  Type System ─────────────┤          │         │       │        │
│       │                   │          │         │       │        │
│       ▼                   ▼          ▼         ▼       ▼        │
│  ┌─────────┐        ┌─────────┐ ┌─────────┐ ┌─────────┐        │
│  │Complete-│        │Refactor-│ │Framework│ │ Debug   │        │
│  │ions     │        │ing      │ │Support  │ │ Adapter │        │
│  └─────────┘        └─────────┘ └─────────┘ └─────────┘        │
│       │                   │          │         │                │
│       └───────────────────┴──────────┴─────────┘                │
│                           │                                      │
│                           ▼                                      │
│                      LSP Server                                  │
│                           │                                      │
│                           ▼                                      │
│                    Editor Extensions                             │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

---

## Milestones

### M1: First Light (Month 3)
- [ ] Parse valid Java files
- [ ] Basic LSP server running
- [ ] Show syntax errors
- [ ] Simple go to definition

### M2: Developer Preview (Month 8)
- [ ] Full Java parsing with recovery
- [ ] Type checking working
- [ ] Code completion useful
- [ ] Navigation features complete
- [ ] Basic diagnostics

### M3: Beta (Month 14)
- [ ] Refactoring working
- [ ] Framework support (Spring, Lombok)
- [ ] Debug adapter functional
- [ ] Performance acceptable
- [ ] Multi-editor support

### M4: 1.0 Release (Month 18)
- [ ] Feature complete
- [ ] Performance targets met
- [ ] Documentation complete
- [ ] Community ready

### M5: Excellence (Month 20+)
- [ ] AI features integrated
- [ ] Performance leadership
- [ ] Ecosystem growing
- [ ] Continuous improvement

---

## Risk Factors

```
┌─────────────────────────────────────────────────────────────────┐
│                    RISK FACTORS                                  │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  TECHNICAL RISKS                                                │
│  • Java complexity: Type system edge cases                      │
│  • Performance: May be harder than expected                     │
│  • Compatibility: Matching javac behavior exactly               │
│  • Framework magic: Reflection-heavy frameworks                 │
│                                                                  │
│  MITIGATION                                                     │
│  • Extensive JLS-based test suite                               │
│  • Early performance benchmarking                               │
│  • Comparison testing against javac                             │
│  • Conservative framework support scope                         │
│                                                                  │
│  RESOURCE RISKS                                                 │
│  • Scope creep                                                  │
│  • Dependency on key individuals                                │
│  • Competing priorities                                         │
│                                                                  │
│  MITIGATION                                                     │
│  • Clear phase boundaries                                       │
│  • Knowledge sharing and documentation                          │
│  • Regular priority reviews                                     │
│                                                                  │
│  ADOPTION RISKS                                                 │
│  • User inertia (switching costs)                               │
│  • Feature gaps vs IntelliJ                                     │
│  • Ecosystem network effects                                    │
│                                                                  │
│  MITIGATION                                                     │
│  • Focus on unique value (performance, editor freedom)          │
│  • Gradual feature parity                                       │
│  • Strong community engagement                                  │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

---

## Success Criteria

```
┌─────────────────────────────────────────────────────────────────┐
│                    SUCCESS CRITERIA                              │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  CORRECTNESS                                                    │
│  • Zero false positive errors (that javac doesn't report)       │
│  • Type checking matches JLS specification                      │
│  • Refactorings preserve program behavior                       │
│                                                                  │
│  PERFORMANCE                                                    │
│  • Completion < 50ms (p95)                                      │
│  • Diagnostics < 100ms after edit                               │
│  • Memory < 2GB for 1M LOC                                      │
│  • Startup < 2 seconds to useful                                │
│                                                                  │
│  FEATURES                                                       │
│  • Completion quality matches IntelliJ                          │
│  • All common refactorings available                            │
│  • Spring/Lombok support working                                │
│  • Debugging functional                                         │
│                                                                  │
│  ADOPTION                                                       │
│  • Used in production by teams                                  │
│  • Positive community feedback                                  │
│  • Growing contributor base                                     │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

---

## Closing Notes

Building a Java language server superior to IntelliJ is among the most ambitious projects in developer tooling. This plan provides a roadmap, but success will require:

1. **Technical Excellence**: Deep expertise in compilers, type systems, and performance engineering
2. **User Focus**: Constant attention to what developers actually need
3. **Persistence**: This is a multi-year journey
4. **Community**: Building an ecosystem of contributors and users

The detailed documents in this folder provide the technical depth needed to execute. The path is clear—the challenge is in the doing.

---

[← Previous: Testing Strategy](14-testing-strategy.md) | [Next: Java Language Levels →](16-java-language-levels.md)
