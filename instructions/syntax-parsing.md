# Syntax & Parsing Workstream

> **⚠️ MANDATORY: Read and follow [AGENTS.md](../AGENTS.md) completely before proceeding.**
> **All rules in AGENTS.md apply at all times. This file adds workstream-specific guidance.**

---

## Scope

This workstream owns Java syntax analysis - lexing, parsing, and syntax tree representation:

| Crate | Purpose |
|-------|---------|
| `nova-syntax` | Lexer, parser, syntax tree (CST), error recovery |
| `nova-format` | Code formatter, pretty printing |

---

## Key Documents

**Required reading:**
- [05 - Syntax and Parsing](../docs/05-syntax-and-parsing.md) - Parser architecture
- [16 - Java Language Levels](../docs/16-java-language-levels.md) - Version-specific syntax

**ADRs:**
- [ADR-0002: Syntax Tree (Rowan)](../docs/adr/0002-syntax-tree-rowan.md)

---

## Architecture

### Green-Red Trees (Rowan)

Nova uses Rowan-style syntax trees:

```
Green Tree (immutable, shared)     Red Tree (with parent pointers)
┌─────────────────────────────┐    ┌─────────────────────────────┐
│ • Structural sharing        │    │ • Created on-demand         │
│ • Cheap cloning             │ ←→ │ • Parent/sibling navigation │
│ • Persistent across edits   │    │ • Position information      │
└─────────────────────────────┘    └─────────────────────────────┘
```

**Key invariants:**
1. Green trees are immutable and can be shared
2. Red trees are cheap wrappers with parent pointers
3. All source text can be reconstructed from the tree
4. Whitespace and comments are preserved (trivia)

### Error Recovery

The parser must handle broken code gracefully:

```java
// User is typing...
public class Foo {
    public void bar() {
        if (x > 
    }
}
```

**Recovery strategies:**
1. Insert missing tokens (`;`, `)`, `}`)
2. Skip unexpected tokens
3. Use error nodes to wrap unparseable regions
4. Never panic or abort - always produce a tree

---

## Development Guidelines

### Adding New Syntax

When adding support for new Java features:

1. **Update lexer** - Add new keywords/tokens
2. **Update grammar** - Add parsing rules
3. **Update AST** - Add typed accessors
4. **Add tests** - Parser tests with expected trees
5. **Update formatter** - Handle pretty-printing

```rust
// Example: Adding record patterns (Java 21)
// 1. Lexer: No new tokens needed
// 2. Parser: Add pattern parsing rule
fn record_pattern(&mut self) -> Option<CompletedMarker> {
    // ...
}
// 3. AST: Add accessor
impl RecordPattern {
    pub fn component_patterns(&self) -> impl Iterator<Item = Pattern> { ... }
}
```

### Test-Driven Development

Parser tests use `.java` input and `.tree` expected output:

```
testdata/
├── parser/
│   ├── expressions/
│   │   ├── binary_ops.java
│   │   └── binary_ops.tree
│   ├── statements/
│   └── declarations/
```

These fixtures are exercised by the `golden_corpus` test inside the consolidated `harness`
integration test binary (`crates/nova-syntax/tests/harness.rs` includes
`crates/nova-syntax/tests/suite/golden_corpus.rs`). There is no separate `--test golden_corpus`
target — run it via `--test harness` and (optionally) a test-name filter.

**To add a test:**
1. Create `testdata/parser/category/test_name.java`
2. Run the golden corpus test with `BLESS=1` once to write the expected outputs
3. Review and commit the generated `.tree` (and `.errors` for recovery fixtures)
4. Re-run without `BLESS=1` to ensure the expectations are stable

```bash
# (Re)generate expected `.tree`/`.errors` outputs
BLESS=1 bash scripts/cargo_agent.sh test -p nova-syntax --test harness suite::golden_corpus

# Run the golden corpus test
bash scripts/cargo_agent.sh test -p nova-syntax --test harness suite::golden_corpus

# Run the full `nova-syntax` integration suite (`harness`)
bash scripts/cargo_agent.sh test -p nova-syntax --test harness
```

### Java Language Levels

Different Java versions have different syntax:

```rust
// Check language level before parsing
if self.language_level >= JavaLevel::Java16 {
    self.parse_record_declaration();
}
```

**Supported versions:** Java 8, 11, 17, 21+

---

## Formatter (nova-format)

The formatter produces canonical Java code:

```rust
let formatted = nova_format::format_file(source, options);
```

**Formatting rules:**
1. Preserve semantics exactly
2. Respect user configuration (indent size, etc.)
3. Handle partial formatting (selection)
4. Work with broken code (format what's parseable)

---

## Testing

```bash
# Parser unit tests
bash scripts/cargo_agent.sh test -p nova-syntax --lib

# Parser integration tests (`harness` includes the `golden_corpus` fixture test)
bash scripts/cargo_agent.sh test -p nova-syntax --test harness

# Parser golden corpus fixtures (test-name filter)
bash scripts/cargo_agent.sh test -p nova-syntax --test harness suite::golden_corpus

# Formatter tests (`insta` snapshots)
bash scripts/cargo_agent.sh test -p nova-format --test harness
# or focus on a subset:
bash scripts/cargo_agent.sh test -p nova-format --test harness suite::format_fixtures
bash scripts/cargo_agent.sh test -p nova-format --test harness suite::format_snapshots

# Update / bless expectations:
BLESS=1 bash scripts/cargo_agent.sh test -p nova-syntax --test harness suite::golden_corpus
INSTA_UPDATE=always bash scripts/cargo_agent.sh test -p nova-format --test harness suite::format_fixtures
INSTA_UPDATE=always bash scripts/cargo_agent.sh test -p nova-format --test harness suite::format_snapshots
```

---

## Common Pitfalls

1. **Breaking error recovery** - Always test with malformed input
2. **Forgetting trivia** - Whitespace/comments must be preserved
3. **Language level gates** - New syntax needs version checks
4. **Performance regression** - Parsing is hot path, benchmark changes

---

## Dependencies

**Upstream:** `nova-core` (FileId, spans)
**Downstream:** All semantic analysis, IDE features, refactoring

Changes to syntax tree structure affect many downstream crates. Coordinate carefully.

---

*Remember: Always follow [AGENTS.md](../AGENTS.md) rules. Use wrapper scripts. Scope your cargo commands.*
