# 08 - Refactoring Engine

[← Back to Main Document](../AGENTS.md) | [Previous: Code Intelligence](07-code-intelligence.md)

## Overview

Safe, automated refactoring is one of IntelliJ's crown jewels. Nova's refactoring engine must match this capability while leveraging our architecture for better performance and new possibilities.

---

## Refactoring Philosophy

```
┌─────────────────────────────────────────────────────────────────┐
│                    REFACTORING PRINCIPLES                        │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  1. SEMANTIC, NOT TEXTUAL                                       │
│     Refactoring operates on program semantics, not text.        │
│     "Rename method foo to bar" updates all semantic references, │
│     not text matches.                                           │
│                                                                  │
│  2. SAFE BY DEFAULT                                              │
│     Every refactoring must preserve program behavior.           │
│     Conflicts and risks are detected before applying.           │
│                                                                  │
│  3. PREVIEWABLE                                                  │
│     Users can see exactly what will change before applying.     │
│     Changes can be selectively included/excluded.               │
│                                                                  │
│  4. UNDOABLE                                                     │
│     All refactorings are undoable as a single operation.        │
│                                                                  │
│  5. COMPOSABLE                                                   │
│     Complex refactorings are built from simpler primitives.     │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

---

## Refactoring Catalog

### Rename

```
┌─────────────────────────────────────────────────────────────────┐
│                    RENAME REFACTORING                            │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  TARGETS                                                        │
│  • Classes, interfaces, enums, records                          │
│  • Methods (including overrides)                                │
│  • Fields                                                       │
│  • Local variables, parameters                                  │
│  • Type parameters                                              │
│  • Packages                                                     │
│                                                                  │
│  SPECIAL HANDLING                                                │
│  • Rename method → rename all overrides                         │
│  • Rename class → rename file (if public and file named)        │
│  • Rename field → rename accessors (if JavaBean pattern)        │
│  • Rename package → move files                                  │
│                                                                  │
│  CONFLICT DETECTION                                              │
│  • Name already exists in scope                                 │
│  • Visibility conflicts after rename                            │
│  • Shadowing introduction                                       │
│  • Binary compatibility break (for libraries)                   │
│                                                                  │
│  OPTIONAL SCOPE                                                  │
│  • Update strings/comments (configurable)                       │
│  • Update test references                                       │
│  • Update configuration files                                   │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Extract Method

```rust
/// Extract method refactoring
pub struct ExtractMethod {
    /// The selection to extract
    selection: TextRange,
    /// The target scope (enclosing class)
    target: ClassId,
    /// New method name
    name: String,
    /// Visibility
    visibility: Visibility,
}

impl ExtractMethod {
    pub fn analyze(&self, db: &dyn Database) -> ExtractMethodAnalysis {
        // 1. Determine parameters (variables read from outside selection)
        let reads = db.reads_in_range(self.selection);
        let parameters: Vec<_> = reads
            .into_iter()
            .filter(|v| !db.defined_in_range(v, self.selection))
            .collect();
        
        // 2. Determine return value (variables written and read after)
        let writes = db.writes_in_range(self.selection);
        let returns: Vec<_> = writes
            .into_iter()
            .filter(|v| db.read_after_range(v, self.selection))
            .collect();
        
        // 3. Check for issues
        let mut issues = Vec::new();
        
        // Multiple returns not directly supported
        if returns.len() > 1 {
            issues.push(ExtractIssue::MultipleReturns(returns.clone()));
        }
        
        // Check for control flow issues (break, continue, return in selection)
        if let Some(cf) = db.control_flow_exits(self.selection) {
            issues.push(ExtractIssue::ControlFlowExit(cf));
        }
        
        // 4. Determine exceptions thrown
        let exceptions = db.exceptions_in_range(self.selection);
        
        ExtractMethodAnalysis {
            parameters,
            returns,
            exceptions,
            issues,
        }
    }
    
    pub fn apply(&self, db: &mut dyn Database) -> WorkspaceEdit {
        let analysis = self.analyze(db);
        
        // Generate new method
        let new_method = self.generate_method(db, &analysis);
        
        // Generate call expression
        let call = self.generate_call(db, &analysis);
        
        // Create edit
        WorkspaceEdit {
            changes: vec![
                // Replace selection with call
                TextEdit::replace(self.selection, call),
                // Insert new method
                TextEdit::insert(self.insertion_point(db), new_method),
            ],
        }
    }
}
```

### Extract Variable

Current implementation policy (semantic + statement-aware):

- **Insertion point:** the new local declaration is inserted immediately **before the _enclosing statement_** that contains the extracted expression (statement-aware), not simply at the start of the current line.
- **Formatting:** newline style (LF vs CRLF) and indentation are preserved. The inserted declaration uses the indentation of the enclosing statement line.
- **Type annotation:**
  - `use_var: true` emits `var <name> = <expr>;`
  - `use_var: false` emits an explicit Java type when we can infer it **best-effort**. If we cannot infer a type confidently, the refactoring is rejected rather than guessing. In practice, clients should default to `use_var: true` for maximum applicability.
    - Current explicit type inference is intentionally conservative and may only infer a small set of simple types (e.g. `int`/`String`/`char`) for literal and basic expression forms.
- **Applicability:** the implementation is intentionally conservative and rejects extractions from contexts where hoisting the expression would change semantics or where we cannot reliably pick a statement insertion point. Known non-applicable contexts include:
  - expressions that are the condition of `while (...) { ... }` loops
  - expressions that are the condition of `do { ... } while (...);` loops
  - expressions that appear in the header of `for (...) { ... }` statements (init/condition/update)
  - files that do not parse cleanly, or selections that do not resolve to a single expression node

API shape (refactoring-engine entrypoint):

```rust
use nova_refactor::{extract_variable, ExtractVariableParams, FileId, WorkspaceTextRange};

let edit = extract_variable(
    db,
    ExtractVariableParams {
        file: FileId::new("file:///Test.java"),
        // Byte range that should correspond to a single expression node.
        expr_range: WorkspaceTextRange::new(expr_start, expr_end),
        name: "extracted".to_string(),
        // `true` => `var`, `false` => best-effort explicit type.
        use_var: true,
        // Replace other equivalent expressions in the same scope (conservative in `switch`).
        replace_all: false,
    },
)?;
```

### Inline Variable

Current implementation policy:

- **Target:** currently limited to *local variables* (not fields/parameters), and only when a suitable initializer is available. The implementation is conservative about supported declaration forms (for example, it may reject variables declared in `for (...)` headers).
- **Two modes:**
  - **Inline at cursor**: inline the single usage that the user invoked the refactoring on.
  - **Inline all usages**: inline every usage of the variable within its scope.
- **Usage selection required:** in “inline at cursor” mode, the API must identify *which* usage to inline (e.g. via a `usage_range`/cursor selection in the params), since a variable may have multiple references.
- **Safety restrictions:** the implementation rejects variables that:
  - do not have an initializer
  - are written to / mutated after initialization (must be effectively final)
  - have an initializer that is not safe to duplicate (side-effectful initializers are rejected, especially when inlining multiple usages)
- **Expression hygiene:** the implementation may introduce parentheses around the inlined initializer to preserve operator precedence and avoid changing evaluation order.
- **Safe deletion rules:** the declaration is removed only when:
  - the refactoring is run in **inline all usages** mode, or
  - the refactoring is run in **inline at cursor** mode *and* that usage is the **last remaining** usage.
  Otherwise, the declaration is kept.

API shape (refactoring-engine entrypoint):

- `symbol`: the local variable to inline
- `inline_all`: whether to inline all usages (`true`) or only the usage at the cursor (`false`)
- when `inline_all == false`, the API must also carry *which usage* to inline (for example a `usage_range`
  byte range, or a cursor position that resolves to a single reference)

Example payloads (field names are illustrative):

```text
inlineVariable({
  symbol: <SymbolId>,
  inlineAll: false,
  usageRange: { start: <byteOffset>, end: <byteOffset> }
})
```

```text
inlineVariable({
  symbol: <SymbolId>,
  inlineAll: true
})
```

### Move

```
┌─────────────────────────────────────────────────────────────────┐
│                    MOVE REFACTORING                              │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  MOVE CLASS                                                      │
│  • Move to different package                                    │
│  • Update all imports                                           │
│  • Update qualified references                                  │
│  • Handle nested classes                                        │
│                                                                  │
│  MOVE INNER TO TOP LEVEL                                        │
│  • Create new file                                              │
│  • Add outer class instance parameter if needed                 │
│  • Update all usages                                            │
│                                                                  │
│  MOVE METHOD                                                     │
│  • Move instance method to parameter/field type                 │
│  • Update method body (this → parameter)                        │
│  • Update all call sites                                        │
│                                                                  │
│  MOVE STATIC MEMBERS                                             │
│  • Move to different class                                      │
│  • Update all qualified accesses                                │
│  • Handle dependencies between moved members                    │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Change Signature

```rust
/// Change method signature refactoring
pub struct ChangeSignature {
    method: MethodId,
    
    /// New name (None = keep current)
    new_name: Option<String>,
    
    /// New visibility (None = keep current)
    new_visibility: Option<Visibility>,
    
    /// Parameter changes
    parameters: Vec<ParameterChange>,
    
    /// New return type (None = keep current)
    new_return_type: Option<Type>,
    
    /// New exceptions (None = keep current)
    new_exceptions: Option<Vec<Type>>,
}

pub enum ParameterChange {
    /// Keep parameter at position
    Keep { index: usize },
    /// Remove parameter at position
    Remove { index: usize },
    /// Add new parameter
    Add { 
        position: usize, 
        name: String, 
        ty: Type,
        default_value: Option<String>,
    },
    /// Reorder parameter
    Move { from: usize, to: usize },
    /// Rename parameter
    Rename { index: usize, new_name: String },
    /// Change parameter type
    ChangeType { index: usize, new_type: Type },
}

impl ChangeSignature {
    pub fn analyze(&self, db: &dyn Database) -> ChangeSignatureAnalysis {
        let overriders = db.overriding_methods(self.method);
        let call_sites = db.find_references(Symbol::Method(self.method), false);
        
        // Check for conflicts
        let mut issues = Vec::new();
        
        // Check parameter removal
        for change in &self.parameters {
            if let ParameterChange::Remove { index } = change {
                // Check if parameter is used in body
                if db.parameter_is_used(self.method, *index) {
                    issues.push(ChangeIssue::RemovedParameterUsed(*index));
                }
            }
        }
        
        // Check type changes for compatibility
        if let Some(new_return) = &self.new_return_type {
            for site in &call_sites {
                if !db.type_compatible_at(new_return, site) {
                    issues.push(ChangeIssue::ReturnTypeIncompatible(site.clone()));
                }
            }
        }
        
        ChangeSignatureAnalysis {
            overriders,
            call_sites,
            issues,
        }
    }
    
    pub fn apply(&self, db: &mut dyn Database) -> WorkspaceEdit {
        let analysis = self.analyze(db);
        let mut edits = Vec::new();
        
        // Update method declaration
        edits.push(self.update_declaration(db));
        
        // Update all overriders
        for overrider in analysis.overriders {
            edits.push(self.update_override(db, overrider));
        }
        
        // Update all call sites
        for site in analysis.call_sites {
            edits.push(self.update_call_site(db, site));
        }
        
        WorkspaceEdit { changes: edits }
    }
}
```

---

## Refactoring Infrastructure

### Semantic Diff

```rust
/// Semantic representation of code changes
pub enum SemanticChange {
    /// Rename a symbol
    Rename {
        symbol: Symbol,
        new_name: String,
    },
    
    /// Move a declaration
    Move {
        declaration: DeclarationId,
        target: MoveTarget,
    },
    
    /// Change type
    ChangeType {
        target: TypeTarget,
        new_type: Type,
    },
    
    /// Add declaration
    Add {
        declaration: Declaration,
        target: Container,
    },
    
    /// Remove declaration
    Remove {
        declaration: DeclarationId,
    },
    
    /// Update references
    UpdateReferences {
        old_symbol: Symbol,
        new_symbol: Symbol,
    },
}

/// Convert semantic changes to text edits
pub fn materialize(
    db: &dyn Database,
    changes: Vec<SemanticChange>,
) -> WorkspaceEdit {
    let mut edits = Vec::new();
    
    for change in changes {
        match change {
            SemanticChange::Rename { symbol, new_name } => {
                // Update definition
                let def_loc = db.symbol_definition(symbol);
                edits.push(TextEdit::replace(def_loc.name_range, new_name.clone()));
                
                // Update all references
                for ref_loc in db.find_references(symbol, false) {
                    edits.push(TextEdit::replace(ref_loc.range, new_name.clone()));
                }
            }
            // ... other change types
        }
    }
    
    // Deduplicate and sort edits
    normalize_edits(&mut edits);
    
    WorkspaceEdit { changes: edits }
}
```

### Conflict Detection

```rust
/// Check for conflicts before applying refactoring
pub fn check_conflicts(
    db: &dyn Database,
    changes: &[SemanticChange],
) -> Vec<Conflict> {
    let mut conflicts = Vec::new();
    
    for change in changes {
        match change {
            SemanticChange::Rename { symbol, new_name } => {
                // Check for name collision in scope
                let scope = db.symbol_scope(*symbol);
                if db.name_exists_in_scope(scope, new_name) {
                    conflicts.push(Conflict::NameCollision {
                        name: new_name.clone(),
                        scope,
                    });
                }
                
                // Check for shadowing
                if let Some(shadowed) = db.would_shadow(scope, new_name) {
                    conflicts.push(Conflict::Shadowing {
                        name: new_name.clone(),
                        shadowed,
                    });
                }
                
                // Check visibility after rename
                let vis = db.symbol_visibility(*symbol);
                for usage in db.find_references(*symbol, false) {
                    if !db.is_visible_from(&vis, &usage.file, new_name) {
                        conflicts.push(Conflict::VisibilityLoss {
                            usage: usage.clone(),
                        });
                    }
                }
            }
            // ... other change types
        }
    }
    
    conflicts
}
```

### Preview Generation

```rust
/// Generate preview of refactoring changes
pub fn generate_preview(
    db: &dyn Database,
    edit: &WorkspaceEdit,
) -> RefactoringPreview {
    let mut file_changes = Vec::new();
    
    // Group edits by file
    let by_file = edit.changes.iter().into_group_map_by(|e| e.file);
    
    for (file, edits) in by_file {
        let original = db.file_content(file);
        let modified = apply_edits(&original, &edits);
        
        // Generate unified diff
        let diff = create_diff(&original, &modified);
        
        file_changes.push(FileChange {
            file,
            original_content: original,
            modified_content: modified,
            diff,
            edit_count: edits.len(),
        });
    }
    
    RefactoringPreview {
        total_files: file_changes.len(),
        total_edits: edit.changes.len(),
        files: file_changes,
    }
}
```

---

## Advanced Refactorings

### Safe Delete

```rust
/// Safely delete a declaration, checking for usages
pub fn safe_delete(
    db: &dyn Database,
    target: DeclarationId,
) -> SafeDeleteResult {
    let references = db.find_references(Symbol::Declaration(target), false);
    
    if references.is_empty() {
        // No usages, safe to delete
        SafeDeleteResult::Safe {
            edit: delete_declaration_edit(db, target),
        }
    } else {
        // Has usages, show to user
        SafeDeleteResult::Unsafe {
            usages: references,
            delete_anyway: Box::new(move |db| {
                // Option to delete references too
                delete_with_usages(db, target, &references)
            }),
        }
    }
}
```

### Introduce Parameter Object

```rust
/// Convert multiple parameters to a parameter object
pub fn introduce_parameter_object(
    db: &dyn Database,
    method: MethodId,
    params: Vec<usize>,  // Parameter indices to include
    class_name: String,
) -> WorkspaceEdit {
    let method_data = db.method(method);
    
    // Generate new class
    let class_def = generate_parameter_class(
        &class_name,
        params.iter().map(|i| &method_data.params[*i]).collect(),
    );
    
    // Update method signature
    let new_param = format!("{} {}", class_name, to_camel_case(&class_name));
    
    // Update method body
    let body_updates = update_parameter_accesses(db, method, &params, &class_name);
    
    // Update all call sites
    let call_updates = update_call_sites_for_parameter_object(
        db, method, &params, &class_name
    );
    
    // Combine all changes
    combine_edits(vec![class_def, method_update, body_updates, call_updates])
}
```

### Convert to Record

```rust
/// Convert a class to a Java record (Java 16+)
pub fn convert_to_record(
    db: &dyn Database,
    class: ClassId,
) -> Result<WorkspaceEdit, ConvertError> {
    let class_data = db.class(class);
    
    // Check eligibility
    if !class_data.extends.is_none() || class_data.extends == Some("Object") {
        return Err(ConvertError::HasSuperclass);
    }
    
    if !class_data.is_final {
        return Err(ConvertError::NotFinal);
    }
    
    // Analyze fields
    let fields: Vec<_> = class_data.fields.iter()
        .filter(|f| !f.is_static)
        .collect();
    
    // Check all fields are final
    if !fields.iter().all(|f| f.is_final) {
        return Err(ConvertError::NonFinalFields);
    }
    
    // Check for canonical constructor or no constructor
    let constructors = db.constructors(class);
    if !is_canonical_or_empty(&constructors, &fields) {
        return Err(ConvertError::NonCanonicalConstructor);
    }
    
    // Generate record
    let record_def = generate_record(
        &class_data.name,
        &fields,
        &class_data.methods.iter().filter(|m| !is_accessor(m, &fields)).collect(),
    );
    
    Ok(WorkspaceEdit {
        changes: vec![TextEdit::replace(class_data.range, record_def)],
    })
}
```

---

## Integration with LSP

```rust
use serde_json::json;

/// Handle code action request for refactoring
pub fn refactoring_code_actions(
    db: &dyn Database,
    file: FileId,
    range: TextRange,
) -> Vec<CodeAction> {
    let mut actions = Vec::new();
    
    // Context-sensitive refactorings
    //
    // Extract Variable typically needs user input (the new variable name), so
    // it is commonly offered as an unresolved code action that stores the
    // selected expression range + options in `data`. The client later resolves
    // the action (e.g. via `codeAction/resolve`) after prompting for a name.
    if let Some(expr_range) = db.expression_range_at(file, range) {
        actions.push(CodeAction {
            title: "Extract variable…".into(),
            kind: Some(CodeActionKind::REFACTOR_EXTRACT),
            data: Some(json!({
                "kind": "extractVariable",
                "exprRange": expr_range,
                "useVar": true,
            })),
            ..CodeAction::default()
        });
    }
    
    if let Some(selection) = db.statement_range_at(file, range) {
        // Extract method available for statements
        actions.push(CodeAction {
            title: "Extract method".into(),
            kind: Some(CodeActionKind::REFACTOR_EXTRACT),
            command: Some(command("extract_method", [selection.into()])),
        });
    }
    
    if let Some(symbol) = db.symbol_at(file, range.start()) {
        // Rename available for any symbol
        actions.push(CodeAction {
            title: "Rename".into(),
            kind: Some(CodeActionKind::REFACTOR_RENAME),
            command: Some(command("rename", [symbol.into()])),
        });
         
        // Inline Variable is surfaced as two actions:
        // - inline at cursor (requires a usage selection)
        // - inline all usages
        if let Some(usage_range) = db.local_usage_range_at(file, range.start()) {
            actions.push(CodeAction {
                title: "Inline variable".into(),
                kind: Some(CodeActionKind::REFACTOR_INLINE),
                data: Some(json!({
                    "kind": "inlineVariable",
                    "symbol": symbol,
                    "inlineAll": false,
                    "usageRange": usage_range,
                })),
                ..CodeAction::default()
            });
            actions.push(CodeAction {
                title: "Inline variable (all usages)".into(),
                kind: Some(CodeActionKind::REFACTOR_INLINE),
                data: Some(json!({
                    "kind": "inlineVariable",
                    "symbol": symbol,
                    "inlineAll": true,
                })),
                ..CodeAction::default()
            });
        }
    }
    
    actions
}
```

---

## Performance Considerations

```
┌─────────────────────────────────────────────────────────────────┐
│                    PERFORMANCE TARGETS                           │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  RENAME                                                          │
│  • 10 references: < 100ms                                       │
│  • 100 references: < 500ms                                      │
│  • 1000 references: < 2s                                        │
│                                                                  │
│  EXTRACT METHOD                                                  │
│  • Analysis: < 100ms                                            │
│  • Application: < 200ms                                         │
│                                                                  │
│  CHANGE SIGNATURE                                                │
│  • Analysis (100 call sites): < 500ms                           │
│  • Application: < 1s                                            │
│                                                                  │
│  PREVIEW GENERATION                                              │
│  • Per file: < 50ms                                             │
│  • Total: < 2s for 100 files                                    │
│                                                                  │
│  OPTIMIZATIONS                                                   │
│  • Parallel reference finding                                   │
│  • Incremental edit generation                                  │
│  • Lazy preview for large refactorings                          │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

---

## Next Steps

1. → [Framework Support](09-framework-support.md): Spring, Jakarta EE integration
2. → [Performance Engineering](10-performance-engineering.md): Optimization strategies

---

[← Previous: Code Intelligence](07-code-intelligence.md) | [Next: Framework Support →](09-framework-support.md)
