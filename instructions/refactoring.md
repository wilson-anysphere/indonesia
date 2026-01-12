# Refactoring Workstream

> **⚠️ MANDATORY: Read and follow [AGENTS.md](../AGENTS.md) completely before proceeding.**
> **All rules in AGENTS.md apply at all times. This file adds workstream-specific guidance.**

---

## Scope

This workstream owns automated code transformations - safe, semantic-aware refactorings:

| Crate | Purpose |
|-------|---------|
| `nova-refactor` | Refactoring engine, semantic diff model, all refactorings |

---

## Key Documents

**Required reading:**
- [08 - Refactoring Engine](../docs/08-refactoring-engine.md) - Architecture and design

---

## Architecture

### Semantic Diff Model

Refactorings produce semantic changes, not text edits:

```
┌─────────────────────────────────────────────────────────────────┐
│                    Semantic Diff Model                           │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  RefactoringResult                                              │
│  ├── SemanticChange: RenameSymbol { from, to }                  │
│  ├── SemanticChange: MoveType { type, new_package }             │
│  ├── SemanticChange: ChangeSignature { method, params }         │
│  └── ...                                                        │
│                                                                  │
│         ↓ Convert to text edits                                 │
│                                                                  │
│  TextEdit[]                                                     │
│  ├── { file: "Foo.java", range: ..., text: "newName" }          │
│  ├── { file: "Bar.java", range: ..., text: "newName" }          │
│  └── ...                                                        │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

**Benefits:**
- Changes can be previewed before applying
- Conflicts detected at semantic level
- Multiple refactorings can be composed
- Easier to test correctness

### Conflict Detection

```rust
pub enum Conflict {
    NameCollision { name: String, location: Location },
    AccessibilityChange { symbol: SymbolId, was: Visibility, now: Visibility },
    BrokenReference { from: Location, to: SymbolId },
    BehaviorChange { description: String },
}
```

---

## Refactorings

### Tier 1: Essential (Must Have)

| Refactoring | Description |
|-------------|-------------|
| **Rename** | Rename any symbol (class, method, field, variable, parameter) |
| **Extract Variable** | Extract expression to local variable |
| **Inline Variable** | Replace variable with its value |
| **Extract Constant** | Extract expression to constant |

### Tier 2: Important

| Refactoring | Description |
|-------------|-------------|
| **Extract Method** | Extract code block to new method |
| **Inline Method** | Replace method call with body |
| **Change Signature** | Add/remove/reorder parameters |
| **Move Class** | Move type to different package |
| **Safe Delete** | Delete with usage check |

### Tier 3: Advanced

| Refactoring | Description |
|-------------|-------------|
| **Extract Interface** | Create interface from class |
| **Pull Up / Push Down** | Move members in hierarchy |
| **Introduce Parameter Object** | Bundle parameters into class |
| **Convert to Record** | Convert class to record (Java 16+) |

---

## Development Guidelines

### Implementing a Refactoring

```rust
pub trait Refactoring {
    /// Check if refactoring is applicable at location
    fn is_applicable(&self, ctx: &RefactorContext) -> bool;
    
    /// Compute semantic changes
    fn compute(&self, ctx: &RefactorContext) -> Result<RefactoringResult, RefactorError>;
    
    /// Detect conflicts before applying
    fn detect_conflicts(&self, result: &RefactoringResult) -> Vec<Conflict>;
}
```

**Implementation steps:**
1. Define applicability check
2. Gather all affected locations (uses, references)
3. Compute semantic changes
4. Detect potential conflicts
5. Generate text edits
6. Add comprehensive tests

### Rename Refactoring

The most important refactoring. Must handle:

```java
// Local variable
void foo() {
    int x = 1;  // ← rename
    int y = x;  // ← update reference
}

// Field with getters/setters
class Foo {
    private String name;  // ← rename
    public String getName() { return name; }  // ← update
    public void setName(String name) { this.name = name; }  // ← update
}

// Method override
class Base {
    void process() {}  // ← rename
}
class Derived extends Base {
    @Override
    void process() {}  // ← must also rename
}
```

### Extract Method

Complex refactoring with many edge cases:

```java
void foo() {
    // What variables are used inside selection?
    // What variables are modified?
    // What is the return value?
    // Are there early returns?
    // What exceptions might be thrown?
    
    int x = 1;
    int y = 2;
    // START SELECTION
    int sum = x + y;
    System.out.println(sum);
    // END SELECTION
    System.out.println(x);
}

// Extract to:
void foo() {
    int x = 1;
    int y = 2;
    extracted(x, y);
    System.out.println(x);
}

void extracted(int x, int y) {
    int sum = x + y;
    System.out.println(sum);
}
```

### Correctness Guarantees

**Refactorings must:**
1. Preserve program behavior (semantic equivalence)
2. Preserve compilability (if code compiled before)
3. Detect and report conflicts
4. Be atomic (all or nothing)

**Testing approach:**
1. Compile code before refactoring
2. Apply refactoring
3. Compile code after
4. Run tests (if available)
5. Compare behavior

---

## Testing

```bash
# Run refactoring tests
bash scripts/cargo_agent.sh test -p nova-refactor --lib
```

### Test Structure

```
testdata/refactor/
├── rename/
│   ├── local_variable.java
│   ├── local_variable.expected.java
│   ├── field_with_accessors.java
│   └── field_with_accessors.expected.java
├── extract_method/
│   ├── simple.java
│   └── simple.expected.java
└── ...
```

---

## Common Pitfalls

1. **Missing references** - Use index to find ALL usages
2. **Shadowing issues** - New names might shadow existing
3. **Access modifier changes** - Moving code may require visibility changes
4. **Override chains** - Renaming method affects entire hierarchy
5. **String literals** - Some refactorings update strings (e.g., reflection)

---

## Dependencies

**Upstream:** `nova-syntax`, `nova-types`, `nova-resolve`, `nova-ide` (for references)
**Downstream:** `nova-lsp` (exposes refactorings as code actions)

---

*Remember: Always follow [AGENTS.md](../AGENTS.md) rules. Use wrapper scripts. Scope your cargo commands.*
