# Semantic Analysis Workstream

> **⚠️ MANDATORY: Read and follow [AGENTS.md](../AGENTS.md) completely before proceeding.**
> **All rules in AGENTS.md apply at all times. This file adds workstream-specific guidance.**

---

## Scope

This workstream owns Java semantic analysis - the type system, name resolution, and flow analysis:

| Crate | Purpose |
|-------|---------|
| `nova-types` | Type representation, subtyping, type inference |
| `nova-types-bridge` | Bridge between syntax types and semantic types |
| `nova-types-signature` | Generic signature parsing (from classfiles) |
| `nova-resolve` | Name resolution, scope management, import resolution |
| `nova-hir` | High-level IR, lowered from syntax tree |
| `nova-flow` | Control flow analysis, definite assignment |
| `nova-modules` | Java module system (JPMS) support |
| `nova-classfile` | Class file parsing (for dependencies) |
| `nova-classpath` | Classpath management, dependency resolution |
| `nova-jdk` | JDK class index, standard library types |

---

## Key Documents

 **Required reading:**
 - [06 - Semantic Analysis](../docs/06-semantic-analysis.md) - Type system design
 - [04 - Incremental Computation](../docs/04-incremental-computation.md) - Query architecture
 - [16 - Java Language Levels](../docs/16-java-language-levels.md) - Version-specific semantics
 - [ADR 0011 — Stable `ClassId` and project-level type environments](../docs/adr/0011-stable-classid-and-project-type-environments.md)
 - [ADR 0012 — `ClassId` stability and interning policy](../docs/adr/0012-classid-interning.md)

---

## Architecture

### Type System

```
┌─────────────────────────────────────────────────────────────────┐
│                      Type Representation                         │
├─────────────────────────────────────────────────────────────────┤
│  Primitive    │  int, long, float, double, boolean, ...         │
│  Class        │  String, List<T>, Map<K,V>                      │
│  Array        │  int[], String[][]                              │
│  TypeVar      │  T, E, K, V (generic parameters)                │
│  Wildcard     │  ?, ? extends T, ? super T                      │
│  Intersection │  T & Serializable                               │
│  Null         │  null type                                      │
│  Error        │  placeholder for unresolved types               │
└─────────────────────────────────────────────────────────────────┘
```

### Name Resolution

```
Resolution order (JLS §6.5):
1. Local variables and parameters
2. Fields (inherited, then enclosing classes)
3. Types (single-type imports, same package, on-demand imports)
4. Packages
```

### Query Structure

```rust
// Type checking is demand-driven
#[salsa::tracked]
fn type_of_expr(db: &dyn Db, expr: ExprId) -> Type {
    // Only computes what's needed
}

#[salsa::tracked]  
fn method_resolution(db: &dyn Db, call: CallExpr) -> Option<MethodId> {
    // Overload resolution
}
```

---

## Development Guidelines

### Adding Type System Features

When implementing new type features:

1. **Representation** - Add to `nova-types` type enum
2. **Subtyping** - Update subtype relation
3. **Inference** - Update inference algorithm if needed
4. **Display** - Add pretty-printing
5. **Tests** - JLS compliance tests

### Handling Errors Gracefully

The type system must work with broken code:

```rust
// GOOD: Return error type, continue analysis
fn type_of_expr(&self, expr: &Expr) -> Type {
    match self.resolve_name(name) {
        Some(symbol) => symbol.ty(),
        None => Type::Error, // Don't bail out
    }
}

// BAD: Panic on error
fn type_of_expr(&self, expr: &Expr) -> Type {
    self.resolve_name(name).unwrap() // Will crash
}
```

### JLS Compliance

Every type rule should cite the JLS section:

```rust
/// Checks assignment compatibility (JLS §5.2)
fn is_assignment_compatible(&self, from: &Type, to: &Type) -> bool {
    // Implementation follows JLS §5.2
}
```

### Generics and Type Inference

Java generics are complex. Key concepts:

```java
// Type parameter bounds
<T extends Comparable<T>>

// Wildcard capture
List<?> list;

// Diamond inference
Map<String, List<Integer>> map = new HashMap<>();

// Lambda parameter inference
list.stream().map(x -> x.toString())
```

**Rules:**
1. Follow JLS inference algorithm precisely
2. Use constraint-based inference
3. Handle inference failures gracefully (use error types)

---

## Testing

```bash
# Type system tests
bash scripts/cargo_agent.sh test --locked -p nova-types --lib

# Resolution tests
bash scripts/cargo_agent.sh test --locked -p nova-resolve --lib

# HIR tests
bash scripts/cargo_agent.sh test --locked -p nova-hir --lib

# Flow analysis tests
bash scripts/cargo_agent.sh test --locked -p nova-flow --lib

# Classpath tests
bash scripts/cargo_agent.sh test --locked -p nova-classpath --lib
```

### JLS Compliance Tests

We maintain a suite of tests derived from the Java Language Specification:

```
testdata/jls/
├── ch05_conversions/
├── ch06_names/
├── ch08_classes/
├── ch15_expressions/
└── ...
```

---

## Common Pitfalls

1. **Forgetting error types** - Must handle unresolved names gracefully
2. **Generic variance** - `List<Object>` is NOT a supertype of `List<String>`
3. **Type erasure** - Generic type info lost at runtime, affects reflection
4. **Raw types** - Legacy code without generics needs special handling
5. **Null safety** - Java has nullable types everywhere

---

## Dependencies

**Upstream:** `nova-core`, `nova-syntax`, `nova-db`
**Downstream:** `nova-ide`, `nova-refactor`, `nova-framework-*`

This is the most technically complex workstream. Changes affect IDE intelligence quality.

---

## Coordination

Type system changes can affect:
- Completion accuracy
- Diagnostic messages
- Refactoring safety
- Framework analysis

Test thoroughly and coordinate with downstream workstreams.

---

*Remember: Always follow [AGENTS.md](../AGENTS.md) rules. Use wrapper scripts. Scope your cargo commands.*
