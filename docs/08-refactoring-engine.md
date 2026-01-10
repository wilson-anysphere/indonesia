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

### Extract Variable/Constant

```rust
/// Extract expression to variable
pub fn extract_variable(
    db: &dyn Database,
    expr: ExprId,
    name: String,
    use_var: bool,  // var vs explicit type
) -> WorkspaceEdit {
    let expr_range = db.expr_range(expr);
    let expr_text = db.expr_text(expr);
    let expr_type = db.type_of(expr);
    
    // Find insertion point (before statement)
    let stmt = db.enclosing_statement(expr);
    let insert_pos = db.statement_start(stmt);
    
    // Determine type annotation
    let type_str = if use_var {
        "var".into()
    } else {
        format_type(&expr_type)
    };
    
    // Generate declaration
    let decl = format!("{} {} = {};\n", type_str, name, expr_text);
    
    // Find all occurrences (for "replace all" option)
    let occurrences = find_equivalent_expressions(db, expr);
    
    WorkspaceEdit {
        changes: vec![
            TextEdit::insert(insert_pos, decl),
            TextEdit::replace(expr_range, name),
            // Optionally replace other occurrences
        ],
    }
}
```

### Inline

```rust
/// Inline variable, method, or constant
pub enum InlineTarget {
    Variable(LocalVarId),
    Method(MethodId),
    Constant(FieldId),
}

pub fn inline(
    db: &dyn Database,
    target: InlineTarget,
    inline_all: bool,
) -> Result<WorkspaceEdit, InlineError> {
    match target {
        InlineTarget::Variable(var) => {
            let init = db.variable_initializer(var)
                .ok_or(InlineError::NoInitializer)?;
            let init_text = db.expr_text(init);
            
            let usages = if inline_all {
                db.find_references(Symbol::Local(var), false)
            } else {
                // Single usage at cursor
                vec![db.current_reference()]
            };
            
            // Check for side effects
            if has_side_effects(db, init) && usages.len() > 1 {
                return Err(InlineError::SideEffects);
            }
            
            let mut edits = Vec::new();
            
            // Replace each usage
            for usage in usages {
                edits.push(TextEdit::replace(usage.range, init_text.clone()));
            }
            
            // Remove declaration if inlining all
            if inline_all {
                edits.push(TextEdit::delete(db.declaration_range(var)));
            }
            
            Ok(WorkspaceEdit { changes: edits })
        }
        
        InlineTarget::Method(method) => {
            let body = db.method_body(method)
                .ok_or(InlineError::NoBody)?;
            
            // Check method is suitable for inlining
            if db.method_is_recursive(method) {
                return Err(InlineError::Recursive);
            }
            
            // Transform body for inline context
            let inlined = transform_for_inline(db, method, body);
            
            // ... similar pattern for finding and replacing usages
            todo!()
        }
        
        InlineTarget::Constant(field) => {
            // Similar to variable
            todo!()
        }
    }
}
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
/// Handle code action request for refactoring
pub fn refactoring_code_actions(
    db: &dyn Database,
    file: FileId,
    range: TextRange,
) -> Vec<CodeAction> {
    let mut actions = Vec::new();
    
    // Context-sensitive refactorings
    if let Some(expr) = db.expression_at(file, range) {
        // Extract variable available for expressions
        actions.push(CodeAction {
            title: "Extract variable".into(),
            kind: CodeActionKind::RefactorExtract,
            command: Some(command("extract_variable", [expr.into()])),
        });
    }
    
    if let Some(selection) = db.statement_range_at(file, range) {
        // Extract method available for statements
        actions.push(CodeAction {
            title: "Extract method".into(),
            kind: CodeActionKind::RefactorExtract,
            command: Some(command("extract_method", [selection.into()])),
        });
    }
    
    if let Some(symbol) = db.symbol_at(file, range.start()) {
        // Rename available for any symbol
        actions.push(CodeAction {
            title: "Rename".into(),
            kind: CodeActionKind::RefactorRename,
            command: Some(command("rename", [symbol.into()])),
        });
        
        // Inline available for variables, methods
        if db.can_inline(symbol) {
            actions.push(CodeAction {
                title: "Inline".into(),
                kind: CodeActionKind::RefactorInline,
                command: Some(command("inline", [symbol.into()])),
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
