# 07 - Code Intelligence

[← Back to Main Document](../AGENTS.md) | [Previous: Semantic Analysis](06-semantic-analysis.md)

## Overview

Code intelligence is what users interact with: completions, diagnostics, navigation, hover information. This is where Nova must match IntelliJ's legendary user experience while leveraging our query-based architecture for superior performance.

**Implementation note:** IDE-facing features are expected to be implemented as incremental queries (see [ADR 0001](adr/0001-incremental-query-engine.md)) executed on read-only database snapshots (see [ADR 0004](adr/0004-concurrency-model.md)). The transport layer that exposes these over LSP is covered in [ADR 0003](adr/0003-protocol-frameworks-lsp-dap.md).

---

## Code Completion

### Completion Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                    COMPLETION PIPELINE                           │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  USER TYPES at position                                         │
│           │                                                      │
│           ▼                                                      │
│  ┌─────────────────┐                                            │
│  │ CONTEXT         │  Determine what kind of completion         │
│  │ ANALYSIS        │  • Expression completion                   │
│  │                 │  • Statement completion                    │
│  │                 │  • Type completion                         │
│  │                 │  • Import completion                       │
│  └────────┬────────┘                                            │
│           │                                                      │
│           ▼                                                      │
│  ┌─────────────────┐                                            │
│  │ CANDIDATE       │  Generate potential completions            │
│  │ GENERATION      │  • Members of receiver type                │
│  │                 │  • Visible symbols in scope                │
│  │                 │  • Importable types                        │
│  │                 │  • Keywords                                │
│  └────────┬────────┘                                            │
│           │                                                      │
│           ▼                                                      │
│  ┌─────────────────┐                                            │
│  │ FILTERING       │  Remove invalid/inapplicable               │
│  │                 │  • Type compatibility                      │
│  │                 │  • Visibility                              │
│  │                 │  • Prefix matching                         │
│  └────────┬────────┘                                            │
│           │                                                      │
│           ▼                                                      │
│  ┌─────────────────┐                                            │
│  │ RANKING         │  Order by relevance                        │
│  │                 │  • Exact prefix match                      │
│  │                 │  • Fuzzy match score                       │
│  │                 │  • Type relevance                          │
│  │                 │  • Recency/frequency                       │
│  │                 │  • ML-based scoring                        │
│  └────────┬────────┘                                            │
│           │                                                      │
│           ▼                                                      │
│  COMPLETION ITEMS returned to editor                            │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Context Detection

```rust
/// Determine completion context from cursor position
#[query]
pub fn completion_context(
    db: &dyn Database,
    file: FileId,
    position: Position,
) -> CompletionContext {
    let tree = db.syntax_tree(file);
    let offset = position.to_offset(&db.file_content(file));
    
    // Find the syntax node at cursor
    let node = tree.token_at_offset(offset)
        .left_biased()
        .and_then(|t| t.parent());
    
    match classify_node(node) {
        // After '.' in expression
        NodeClass::DotAccess(receiver) => {
            let recv_type = db.type_of(receiver);
            CompletionContext::MemberAccess { receiver_type: recv_type }
        }
        
        // After '::' in method reference
        NodeClass::MethodReference(type_ref) => {
            CompletionContext::MethodReference { type_ref }
        }
        
        // Inside statement, no specific context
        NodeClass::Statement => {
            CompletionContext::Statement { scope: current_scope(node) }
        }
        
        // Type position (e.g., after ':' in field declaration)
        NodeClass::TypeContext => {
            CompletionContext::Type { scope: current_scope(node) }
        }
        
        // Import statement
        NodeClass::Import => {
            CompletionContext::Import { partial_path: extract_path(node) }
        }
        
        // Annotation
        NodeClass::Annotation => {
            CompletionContext::Annotation
        }
        
        // Inside argument list
        NodeClass::Argument { call, index } => {
            CompletionContext::Argument {
                expected_type: expected_arg_type(db, call, index),
                scope: current_scope(node),
            }
        }
        
        _ => CompletionContext::Expression { scope: current_scope(node) }
    }
}
```

### Member Completion

```rust
/// Generate completions for member access (receiver.xxx)
#[query]
pub fn member_completions(
    db: &dyn Database,
    receiver_type: Type,
    prefix: &str,
) -> Vec<CompletionItem> {
    let mut items = Vec::new();
    
    // Get all accessible members
    for member in db.accessible_members(&receiver_type) {
        match member {
            Member::Field(field) => {
                let field_data = db.field(field);
                if fuzzy_match(prefix, &field_data.name) {
                    items.push(CompletionItem {
                        label: field_data.name.clone(),
                        kind: CompletionKind::Field,
                        detail: Some(format_type(&field_data.ty)),
                        insert_text: field_data.name.clone(),
                        ..Default::default()
                    });
                }
            }
            
            Member::Method(method) => {
                let method_data = db.method(method);
                if fuzzy_match(prefix, &method_data.name) {
                    items.push(CompletionItem {
                        label: method_data.name.clone(),
                        kind: CompletionKind::Method,
                        detail: Some(format_signature(&method_data)),
                        insert_text: format_method_snippet(&method_data),
                        insert_text_format: InsertTextFormat::Snippet,
                        ..Default::default()
                    });
                }
            }
        }
    }
    
    items
}
```

### Smart Completion

```
┌─────────────────────────────────────────────────────────────────┐
│                    SMART COMPLETION                              │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  Context: Assignment to String variable                         │
│  String result = |                                              │
│                                                                  │
│  Regular completion: All visible symbols                        │
│  Smart completion: Only String-typed expressions                │
│                                                                  │
│  Offered:                                                       │
│  • String variables in scope                                    │
│  • Methods returning String                                     │
│  • String constructors                                          │
│  • String literals                                              │
│  • Expressions convertible to String                            │
│                                                                  │
│  Implementation:                                                 │
│  1. Determine expected type from context                        │
│  2. Filter/rank by type compatibility                           │
│  3. Consider conversions (toString(), boxing, etc.)             │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Postfix Completion

```rust
/// Postfix completions transform expressions
pub fn postfix_completions(
    db: &dyn Database,
    expr: ExprId,
    prefix: &str,
) -> Vec<CompletionItem> {
    let expr_type = db.type_of(expr);
    let expr_text = db.expr_text(expr);
    
    let mut items = Vec::new();
    
    // .if → if (expr) { }
    if fuzzy_match(prefix, "if") && expr_type.is_boolean() {
        items.push(postfix_item("if", 
            format!("if ({}) {{\n    $0\n}}", expr_text)));
    }
    
    // .not → !expr
    if fuzzy_match(prefix, "not") && expr_type.is_boolean() {
        items.push(postfix_item("not", format!("!{}", expr_text)));
    }
    
    // .var → var name = expr
    if fuzzy_match(prefix, "var") {
        items.push(postfix_item("var", 
            format!("var ${{1:name}} = {};$0", expr_text)));
    }
    
    // .null → if (expr == null) { }
    if fuzzy_match(prefix, "null") && expr_type.is_reference() {
        items.push(postfix_item("null", 
            format!("if ({} == null) {{\n    $0\n}}", expr_text)));
    }
    
    // .nn (not null) → if (expr != null) { }
    if fuzzy_match(prefix, "nn") && expr_type.is_reference() {
        items.push(postfix_item("nn", 
            format!("if ({} != null) {{\n    $0\n}}", expr_text)));
    }
    
    // .for → for (Type item : expr) { }
    if fuzzy_match(prefix, "for") && expr_type.is_iterable() {
        let elem_type = db.iterable_element_type(&expr_type);
        items.push(postfix_item("for", 
            format!("for ({} ${{1:item}} : {}) {{\n    $0\n}}", 
                format_type(&elem_type), expr_text)));
    }
    
    // .stream → expr.stream()
    if fuzzy_match(prefix, "stream") && expr_type.is_collection() {
        items.push(postfix_item("stream", format!("{}.stream()", expr_text)));
    }
    
    items
}
```

---

## Diagnostics

### Diagnostic Categories

```
┌─────────────────────────────────────────────────────────────────┐
│                    DIAGNOSTIC TYPES                              │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  ERRORS (Must fix)                                              │
│  • Syntax errors                                                │
│  • Type mismatches                                              │
│  • Undefined symbols                                            │
│  • Missing methods (abstract class)                             │
│  • Incompatible types in assignment                             │
│  • Method not found                                             │
│                                                                  │
│  WARNINGS (Should investigate)                                  │
│  • Unused variables                                             │
│  • Deprecated API usage                                         │
│  • Possible null pointer                                        │
│  • Unchecked casts                                              │
│  • Raw type usage                                               │
│                                                                  │
│  HINTS (Suggestions)                                            │
│  • Can use diamond operator                                     │
│  • Can use var                                                  │
│  • Expression can be lambda                                     │
│  • Redundant type cast                                          │
│                                                                  │
│  FRAMEWORK-SPECIFIC                                              │
│  • Spring: Missing bean definition                              │
│  • Spring: Invalid autowiring                                   │
│  • JPA: Invalid entity mapping                                  │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Diagnostic Computation

```rust
#[query]
pub fn file_diagnostics(
    db: &dyn Database,
    file: FileId,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    
    // 1. Syntax errors from parsing
    diagnostics.extend(
        db.parse(file).errors.iter().map(|e| e.into())
    );
    
    // 2. Resolution errors
    for usage in db.unresolved_references(file) {
        diagnostics.push(Diagnostic {
            range: usage.range,
            severity: Severity::Error,
            message: format!("Cannot resolve symbol '{}'", usage.name),
            code: "unresolved-reference",
            related: suggest_similar_names(db, &usage),
        });
    }
    
    // 3. Type errors
    for expr in db.file_expressions(file) {
        if let Type::Error = db.type_of(expr) {
            if let Some(error) = db.type_error(expr) {
                diagnostics.push(error.into());
            }
        }
    }
    
    // 4. Flow analysis errors
    diagnostics.extend(db.definite_assignment_errors(file));
    diagnostics.extend(db.unreachable_code_warnings(file));
    
    // 5. Style/lint warnings
    diagnostics.extend(db.lint_warnings(file));
    
    // 6. Framework-specific diagnostics
    for analyzer in db.framework_analyzers() {
        diagnostics.extend(analyzer.diagnostics(db, file));
    }
    
    diagnostics
}
```

### Quick Fixes

```rust
/// Generate quick fixes for diagnostics
pub fn quick_fixes(
    db: &dyn Database,
    diagnostic: &Diagnostic,
) -> Vec<CodeAction> {
    match diagnostic.code {
        "unresolved-reference" => {
            let name = extract_name(diagnostic);
            let mut fixes = Vec::new();
            
            // Suggest imports
            for importable in db.find_importable_types(&name) {
                fixes.push(CodeAction {
                    title: format!("Import '{}'", importable.qualified_name),
                    kind: CodeActionKind::QuickFix,
                    edit: add_import_edit(db, importable),
                });
            }
            
            // Suggest creating the symbol
            fixes.push(CodeAction {
                title: format!("Create class '{}'", name),
                kind: CodeActionKind::QuickFix,
                edit: create_class_edit(db, &name),
            });
            
            fixes
        }
        
        "type-mismatch" => {
            let (expected, actual) = extract_types(diagnostic);
            let mut fixes = Vec::new();
            
            // Suggest cast
            if db.is_castable(&actual, &expected) {
                fixes.push(CodeAction {
                    title: format!("Add cast to {}", format_type(&expected)),
                    kind: CodeActionKind::QuickFix,
                    edit: add_cast_edit(diagnostic.range, &expected),
                });
            }
            
            // Suggest conversion method
            if let Some(method) = find_conversion_method(db, &actual, &expected) {
                fixes.push(CodeAction {
                    title: format!("Convert using .{}()", method.name),
                    kind: CodeActionKind::QuickFix,
                    edit: add_method_call_edit(diagnostic.range, &method.name),
                });
            }
            
            fixes
        }
        
        "unused-import" => {
            vec![CodeAction {
                title: "Remove unused import".into(),
                kind: CodeActionKind::QuickFix,
                edit: remove_line_edit(diagnostic.range),
            }]
        }
        
        _ => vec![],
    }
}
```

---

## Navigation

### Go to Definition

```rust
#[query]
pub fn definition(
    db: &dyn Database,
    file: FileId,
    position: Position,
) -> Option<Location> {
    let reference = db.reference_at(file, position)?;
    
    match reference {
        Reference::Local(var) => {
            let decl = db.local_declaration(var);
            Some(Location::new(file, decl.range))
        }
        
        Reference::Field(field) => {
            let def = db.field_definition(field);
            Some(Location::new(def.file, def.range))
        }
        
        Reference::Method(method) => {
            let def = db.method_definition(method);
            Some(Location::new(def.file, def.range))
        }
        
        Reference::Type(type_id) => {
            let def = db.type_definition(type_id);
            Some(Location::new(def.file, def.range))
        }
        
        Reference::Package(pkg) => {
            // Navigate to package-info.java or first class
            db.package_location(pkg)
        }
    }
}
```

### Find References

```rust
#[query]
pub fn find_references(
    db: &dyn Database,
    symbol: Symbol,
    include_declaration: bool,
) -> Vec<Location> {
    let mut locations = Vec::new();
    
    // Include declaration if requested
    if include_declaration {
        if let Some(decl) = db.symbol_declaration(symbol) {
            locations.push(decl);
        }
    }
    
    // Use index for initial candidates
    let candidates = db.reference_index().get(&symbol);
    
    // Verify each candidate (index might be stale)
    for (file, range) in candidates {
        // Re-resolve to confirm it references our symbol
        if let Some(ref_sym) = db.symbol_at(file, range.start()) {
            if ref_sym == symbol {
                locations.push(Location::new(file, range));
            }
        }
    }
    
    locations
}
```

### Type Hierarchy

```rust
#[query]
pub fn type_hierarchy(
    db: &dyn Database,
    class_id: ClassId,
) -> TypeHierarchy {
    let type_def = db.type_definition(class_id);
    
    TypeHierarchy {
        item: type_hierarchy_item(db, class_id),
        
        // Supertypes
        supertypes: db.supertypes(class_id)
            .into_iter()
            .map(|t| type_hierarchy_item(db, t))
            .collect(),
        
        // Subtypes (from index)
        subtypes: db.inheritance_index()
            .subtypes_of(class_id)
            .into_iter()
            .map(|t| type_hierarchy_item(db, t))
            .collect(),
    }
}
```

**Implementation note (current repo):** the concrete type system represents classes with `ClassId`
(`nova_types::Type::Class`). Correctness and incremental caching depend on class ids being stable
across bodies and queries; see ADR 0011 and ADR 0012 in `docs/adr/`.

### Call Hierarchy

```rust
#[query]
pub fn incoming_calls(
    db: &dyn Database,
    method: MethodId,
) -> Vec<CallHierarchyItem> {
    // Find all calls to this method
    let references = db.find_references(Symbol::Method(method), false);
    
    references.into_iter()
        .filter_map(|loc| {
            // Find enclosing method
            let enclosing = db.enclosing_method(loc.file, loc.range.start())?;
            Some(CallHierarchyItem {
                name: db.method_name(enclosing),
                kind: SymbolKind::Method,
                uri: loc.file,
                range: db.method_range(enclosing),
                selection_range: db.method_name_range(enclosing),
            })
        })
        .collect()
}

#[query]
pub fn outgoing_calls(
    db: &dyn Database,
    method: MethodId,
) -> Vec<CallHierarchyItem> {
    let body = db.method_body(method);
    
    // Find all method calls in body
    body.method_calls()
        .filter_map(|call| {
            let target = db.resolve_method_call(call)?;
            Some(CallHierarchyItem {
                name: db.method_name(target),
                kind: SymbolKind::Method,
                uri: db.method_file(target),
                range: db.method_range(target),
                selection_range: db.method_name_range(target),
            })
        })
        .collect()
}
```

---

## Hover Information

```rust
#[query]
pub fn hover(
    db: &dyn Database,
    file: FileId,
    position: Position,
) -> Option<Hover> {
    let node = db.node_at(file, position)?;
    
    match classify_for_hover(node) {
        HoverTarget::Type(type_ref) => {
            let resolved = db.resolve_type(type_ref);
            Some(Hover {
                contents: format_type_hover(db, resolved),
                range: type_ref.range(),
            })
        }
        
        HoverTarget::Expression(expr) => {
            let ty = db.type_of(expr);
            let inferred = is_inferred_type(expr);
            
            Some(Hover {
                contents: format_expr_hover(db, expr, &ty, inferred),
                range: expr.range(),
            })
        }
        
        HoverTarget::Method(method) => {
            let method_data = db.method(method);
            Some(Hover {
                contents: format_method_hover(db, &method_data),
                range: node.range(),
            })
        }
        
        HoverTarget::Field(field) => {
            let field_data = db.field(field);
            Some(Hover {
                contents: format_field_hover(db, &field_data),
                range: node.range(),
            })
        }
        
        HoverTarget::LocalVar(var) => {
            let var_type = db.local_var_type(var);
            Some(Hover {
                contents: format_local_hover(db, var, &var_type),
                range: node.range(),
            })
        }
        
        _ => None,
    }
}

fn format_method_hover(db: &dyn Database, method: &Method) -> MarkupContent {
    let mut content = String::new();
    
    // Signature
    content.push_str("```java\n");
    content.push_str(&format_signature(method));
    content.push_str("\n```\n\n");
    
    // Javadoc
    if let Some(doc) = db.javadoc(method.id) {
        content.push_str(&format_javadoc(&doc));
    }
    
    // Declared in
    let class = db.containing_class(method.id);
    content.push_str(&format!("\n*Declared in*: `{}`", db.class_name(class)));
    
    MarkupContent {
        kind: MarkupKind::Markdown,
        value: content,
    }
}
```

---

## Inlay Hints

```rust
#[query]
pub fn inlay_hints(
    db: &dyn Database,
    file: FileId,
    range: TextRange,
) -> Vec<InlayHint> {
    let mut hints = Vec::new();
    
    // Type hints for var declarations
    for var_decl in db.var_declarations_in_range(file, range) {
        if var_decl.is_var {
            let ty = db.local_var_type(var_decl.id);
            hints.push(InlayHint {
                position: var_decl.name_end,
                label: format!(": {}", format_type(&ty)),
                kind: InlayHintKind::Type,
            });
        }
    }
    
    // Parameter name hints
    for call in db.method_calls_in_range(file, range) {
        if should_show_param_hints(&call) {
            let method = db.resolve_method_call(call.id);
            if let Some(method) = method {
                for (i, arg) in call.args.iter().enumerate() {
                    let param_name = db.method_param_name(method, i);
                    if let Some(name) = param_name {
                        hints.push(InlayHint {
                            position: arg.start(),
                            label: format!("{}:", name),
                            kind: InlayHintKind::Parameter,
                        });
                    }
                }
            }
        }
    }
    
    // Chain hints for long method chains
    for chain in db.method_chains_in_range(file, range) {
        if chain.length >= 3 {
            for (i, call) in chain.calls.iter().enumerate() {
                if i > 0 {
                    let ty = db.type_after_call(call);
                    hints.push(InlayHint {
                        position: call.end(),
                        label: format_type_short(&ty),
                        kind: InlayHintKind::Type,
                    });
                }
            }
        }
    }
    
    hints
}
```

---

## Semantic Highlighting

```rust
#[query]
pub fn semantic_tokens(
    db: &dyn Database,
    file: FileId,
) -> Vec<SemanticToken> {
    let mut tokens = Vec::new();
    
    for token in db.syntax_tree(file).tokens() {
        let semantic_type = match token.kind() {
            SyntaxKind::Identifier => {
                // Resolve to determine semantic type
                match db.classify_identifier(file, token.range()) {
                    IdentifierKind::Class => SemanticTokenType::Class,
                    IdentifierKind::Interface => SemanticTokenType::Interface,
                    IdentifierKind::Enum => SemanticTokenType::Enum,
                    IdentifierKind::TypeParameter => SemanticTokenType::TypeParameter,
                    IdentifierKind::Method => SemanticTokenType::Method,
                    IdentifierKind::Property => SemanticTokenType::Property,
                    IdentifierKind::Variable => SemanticTokenType::Variable,
                    IdentifierKind::Parameter => SemanticTokenType::Parameter,
                    IdentifierKind::EnumMember => SemanticTokenType::EnumMember,
                    IdentifierKind::Annotation => SemanticTokenType::Decorator,
                    IdentifierKind::Unknown => continue,
                }
            }
            _ => continue,
        };
        
        let modifiers = compute_modifiers(db, file, token.range());
        
        tokens.push(SemanticToken {
            range: token.range(),
            token_type: semantic_type,
            modifiers,
        });
    }
    
    tokens
}
```

---

## Performance Targets

```
┌─────────────────────────────────────────────────────────────────┐
│                    PERFORMANCE TARGETS                           │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  COMPLETION                                                      │
│  • Trigger to first result: < 50ms                              │
│  • Full list: < 100ms                                           │
│  • With 100+ items: < 150ms                                     │
│                                                                  │
│  DIAGNOSTICS                                                     │
│  • After keystroke: < 100ms (incremental)                       │
│  • Full file: < 500ms                                           │
│                                                                  │
│  NAVIGATION                                                      │
│  • Go to definition: < 50ms                                     │
│  • Find references (100 refs): < 200ms                          │
│  • Find implementations: < 100ms                                │
│                                                                  │
│  HOVER                                                           │
│  • Simple hover: < 20ms                                         │
│  • With type inference: < 50ms                                  │
│                                                                  │
│  INLAY HINTS                                                     │
│  • Visible range: < 50ms                                        │
│                                                                  │
│  SEMANTIC TOKENS                                                 │
│  • Full file (1000 lines): < 100ms                              │
│  • Incremental update: < 20ms                                   │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

---

## Next Steps

1. → [Refactoring Engine](08-refactoring-engine.md): Safe code transformations
2. → [Framework Support](09-framework-support.md): Spring, Jakarta EE support

---

[← Previous: Semantic Analysis](06-semantic-analysis.md) | [Next: Refactoring Engine →](08-refactoring-engine.md)
