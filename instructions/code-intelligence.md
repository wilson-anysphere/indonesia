# Code Intelligence Workstream

> **⚠️ MANDATORY: Read and follow [AGENTS.md](../AGENTS.md) completely before proceeding.**
> **All rules in AGENTS.md apply at all times. This file adds workstream-specific guidance.**

---

## Scope

This workstream owns user-facing IDE features - the intelligence that makes developers productive:

| Crate | Purpose |
|-------|---------|
| `nova-ide` | Core IDE features: completion, navigation, hover, etc. |
| `nova-index` | Symbol indexing, search infrastructure |
| `nova-fuzzy` | Fuzzy matching for completion ranking |

---

## Key Documents

**Required reading:**
- [07 - Code Intelligence](../docs/07-code-intelligence.md) - Feature specifications
- [10 - Performance Engineering](../docs/10-performance-engineering.md) - Latency requirements

---

## Features

### Code Completion

```
┌─────────────────────────────────────────────────────────────────┐
│                    Completion Pipeline                           │
├─────────────────────────────────────────────────────────────────┤
│  1. Context Analysis    │  Where is cursor? What's expected?    │
│  2. Candidate Gathering │  Collect all possible completions     │
│  3. Type Filtering      │  Remove type-incompatible items       │
│  4. Ranking             │  Sort by relevance, recency, etc.     │
│  5. Presentation        │  Format for display                   │
└─────────────────────────────────────────────────────────────────┘
```

**Completion contexts:**
- Member access: `foo.` → fields, methods
- Type context: `Foo x = new ` → constructors
- Import: `import java.util.` → classes, packages
- Keyword: `for (` → snippet templates
- Postfix: `expr.if` → `if (expr) {}`

**Performance target:** <50ms p95

### Navigation

| Feature | Query |
|---------|-------|
| Go to Definition | `definition_at(file, offset)` |
| Find References | `references_to(symbol)` |
| Find Implementations | `implementations_of(type)` |
| Type Hierarchy | `type_hierarchy(type)` |
| Call Hierarchy | `callers_of(method)` / `callees_of(method)` |

### Diagnostics

```rust
pub struct Diagnostic {
    pub range: TextRange,
    pub severity: Severity,
    pub message: String,
    pub code: Option<DiagnosticCode>,
    pub related: Vec<RelatedInformation>,
}
```

**Categories:**
- Syntax errors (from parser)
- Type errors (from type checker)
- Resolution errors (unresolved names)
- Warnings (unused, deprecation)
- Hints (suggestions)

### Hover Information

```rust
// Returns markdown-formatted hover content
fn hover_at(db: &dyn Db, file: FileId, offset: u32) -> Option<HoverResult> {
    // Type info, docs, signature
}
```

### Code Actions

```rust
pub enum CodeAction {
    QuickFix(QuickFix),        // Fix an error
    Refactor(RefactorAction),  // Transform code
    Source(SourceAction),      // Generate code
}
```

---

## Development Guidelines

### Latency Requirements

IDE features must be fast:

| Feature | Target | Max |
|---------|--------|-----|
| Completion | <50ms | 100ms |
| Hover | <20ms | 50ms |
| Go to Definition | <20ms | 50ms |
| Find References | <100ms | 500ms |
| Diagnostics | <100ms after edit | 200ms |

**Rules:**
1. Use indexes for O(1) lookups
2. Limit result sets (pagination)
3. Cancel outdated requests
4. Show partial results early

### Context Detection

Completion quality depends on context detection:

```rust
enum CompletionContext {
    MemberAccess { receiver: Type },
    TypePosition { expected: Option<Type> },
    Statement,
    Expression { expected: Option<Type> },
    Import { prefix: String },
    Annotation,
    // ...
}
```

### Ranking

Good ranking is critical for completion UX:

```rust
fn rank_completion(item: &CompletionItem, ctx: &Context) -> Score {
    let mut score = Score::default();
    
    // Type match bonus
    if ctx.expected_type.matches(&item.ty) {
        score += TYPE_MATCH_BONUS;
    }
    
    // Recency bonus
    if recently_used.contains(&item.name) {
        score += RECENCY_BONUS;
    }
    
    // Fuzzy match score
    score += fuzzy_match_score(&ctx.prefix, &item.name);
    
    score
}
```

### Indexing

The symbol index powers navigation and search:

```rust
// Index structure
pub struct SymbolIndex {
    // Name → Symbol locations
    by_name: HashMap<SmolStr, Vec<SymbolLocation>>,
    // Type → Implementations
    implementations: HashMap<ClassId, Vec<ClassId>>,
    // Method → Callers
    call_graph: HashMap<MethodId, Vec<CallSite>>,
}
```

**Index updates:**
1. Incremental - only reindex changed files
2. Background - don't block user actions
3. Persistent - survive restarts

---

## Testing

```bash
# IDE feature tests
bash scripts/cargo_agent.sh test --locked -p nova-ide --lib

# Index tests
bash scripts/cargo_agent.sh test --locked -p nova-index --lib

# Fuzzy matching tests
bash scripts/cargo_agent.sh test --locked -p nova-fuzzy --lib
```

### Completion Tests

```rust
#[test]
fn test_member_completion() {
    let code = r#"
        class Foo {
            void bar() {
                String s = "";
                s.$0  // cursor position
            }
        }
    "#;
    
    let completions = complete_at(code);
    assert!(completions.iter().any(|c| c.label == "length"));
    assert!(completions.iter().any(|c| c.label == "charAt"));
}
```

---

## Common Pitfalls

1. **Slow completion** - Profile and optimize hot paths
2. **Missing context** - Handle edge cases (partial expressions)
3. **Ranking regressions** - Changes affect user experience
4. **Stale indexes** - Ensure incremental updates work

---

## Dependencies

**Upstream:** `nova-syntax`, `nova-types`, `nova-resolve`
**Downstream:** `nova-lsp`, `nova-refactor`

---

*Remember: Always follow [AGENTS.md](../AGENTS.md) rules. Use wrapper scripts. Scope your cargo commands.*
