# 06 - Semantic Analysis

[← Back to Main Document](../AGENTS.md) | [Previous: Syntax and Parsing](05-syntax-and-parsing.md)

## Overview

Semantic analysis transforms raw syntax trees into meaningful program understanding. This is where Nova must match and exceed IntelliJ's legendary code intelligence. This document covers name resolution, type checking, type inference, and flow analysis.

---

## Semantic Analysis Pipeline

```
┌─────────────────────────────────────────────────────────────────┐
│                    SEMANTIC PIPELINE                             │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  SYNTAX TREE                                                    │
│       │                                                          │
│       ▼                                                          │
│  ┌─────────────────┐                                            │
│  │ 1. ITEM TREE    │  Extract declarations: classes, methods,   │
│  │    LOWERING     │  fields, imports (file-level structure)    │
│  └────────┬────────┘                                            │
│           │                                                      │
│           ▼                                                      │
│  ┌─────────────────┐                                            │
│  │ 2. NAME         │  Resolve identifiers to declarations       │
│  │    RESOLUTION   │  Build scope trees, handle imports         │
│  └────────┬────────┘                                            │
│           │                                                      │
│           ▼                                                      │
│  ┌─────────────────┐                                            │
│  │ 3. TYPE         │  Resolve type references to type defs      │
│  │    RESOLUTION   │  Handle generics, arrays, wildcards        │
│  └────────┬────────┘                                            │
│           │                                                      │
│           ▼                                                      │
│  ┌─────────────────┐                                            │
│  │ 4. TYPE         │  Verify type correctness                   │
│  │    CHECKING     │  Method call resolution, overloading       │
│  └────────┬────────┘                                            │
│           │                                                      │
│           ▼                                                      │
│  ┌─────────────────┐                                            │
│  │ 5. TYPE         │  Infer var types, lambda parameters,       │
│  │    INFERENCE    │  generic type arguments                    │
│  └────────┬────────┘                                            │
│           │                                                      │
│           ▼                                                      │
│  ┌─────────────────┐                                            │
│  │ 6. FLOW         │  Definite assignment, reachability,        │
│  │    ANALYSIS     │  null analysis, exception flow             │
│  └─────────────────┘                                            │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

---

## Item Trees

### Purpose

Item trees provide a stable, file-local view of declarations without full semantic analysis:

```rust
/// File-level item tree (computed once per file)
pub struct ItemTree {
    /// Top-level items in file order
    items: Vec<Item>,
    
    /// Imports in this file
    imports: Vec<Import>,
    
    /// Package declaration
    package: Option<PackageName>,
}

pub enum Item {
    Class(ClassItem),
    Interface(InterfaceItem),
    Enum(EnumItem),
    Record(RecordItem),
    Annotation(AnnotationItem),
}

pub struct ClassItem {
    pub name: Name,
    pub visibility: Visibility,
    pub modifiers: Modifiers,
    pub type_params: Vec<TypeParam>,
    pub extends: Option<TypeRef>,
    pub implements: Vec<TypeRef>,
    pub members: Vec<Member>,
}
```

### Query Definition

```rust
#[query]
pub fn item_tree(db: &dyn Database, file: FileId) -> Arc<ItemTree> {
    let parse = db.parse(file);
    Arc::new(ItemTree::from_syntax(&parse.syntax_tree()))
}

// Item trees are stable: same source → same item tree
// This enables early cutoff for many downstream queries
```

---

## Name Resolution

### Scope Model

```
┌─────────────────────────────────────────────────────────────────┐
│                    SCOPE HIERARCHY                               │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  UNIVERSE SCOPE (java.lang.*, primitive types)                  │
│       │                                                          │
│       ▼                                                          │
│  PACKAGE SCOPE (types in same package)                          │
│       │                                                          │
│       ▼                                                          │
│  IMPORT SCOPE (explicit imports, star imports)                  │
│       │                                                          │
│       ▼                                                          │
│  FILE SCOPE (top-level types in current file)                   │
│       │                                                          │
│       ▼                                                          │
│  CLASS SCOPE (members of enclosing class)                       │
│       │                                                          │
│       ▼                                                          │
│  METHOD SCOPE (parameters, type parameters)                     │
│       │                                                          │
│       ▼                                                          │
│  BLOCK SCOPE (local variables)                                  │
│       │                                                          │
│       ▼                                                          │
│  NESTED BLOCK SCOPES (for/while/try blocks)                     │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Resolution Algorithm

```rust
/// Resolve a simple name to its declaration
#[query]
pub fn resolve_name(
    db: &dyn Database,
    scope: ScopeId,
    name: Name,
) -> Option<Resolution> {
    let scope_data = db.scope(scope);
    
    // 1. Check current scope
    if let Some(local) = scope_data.locals.get(&name) {
        return Some(Resolution::Local(*local));
    }
    
    // 2. Check enclosing scopes
    if let Some(parent) = scope_data.parent {
        if let Some(res) = db.resolve_name(parent, name.clone()) {
            return Some(res);
        }
    }
    
    // 3. Check class members (if in class scope)
    if let Some(class) = scope_data.enclosing_class {
        if let Some(member) = db.resolve_member(class, name.clone()) {
            return Some(Resolution::Member(member));
        }
    }
    
    // 4. Check imports
    if let Some(file) = scope_data.file {
        if let Some(imported) = db.resolve_import(file, name.clone()) {
            return Some(Resolution::Imported(imported));
        }
    }
    
    // 5. Check java.lang
    if let Some(jlang) = db.java_lang_type(&name) {
        return Some(Resolution::JavaLang(jlang));
    }
    
    None
}
```

### Import Resolution

```rust
/// Import resolution supporting all Java import forms
#[query]
pub fn resolve_import(
    db: &dyn Database,
    file: FileId,
    name: Name,
) -> Option<TypeId> {
    let item_tree = db.item_tree(file);
    
    // Check single-type imports first (they shadow star imports)
    for import in &item_tree.imports {
        if let Import::Single { path, alias } = import {
            let import_name = alias.as_ref().unwrap_or(&path.last());
            if import_name == &name {
                return db.resolve_qualified_type(path);
            }
        }
    }
    
    // Check star imports
    for import in &item_tree.imports {
        if let Import::Star { package } = import {
            if let Some(ty) = db.resolve_type_in_package(package, &name) {
                return Some(ty);
            }
        }
    }
    
    // Check same-package types
    let package = item_tree.package.as_ref();
    if let Some(ty) = db.resolve_type_in_package(package, &name) {
        return Some(ty);
    }
    
    None
}
```

---

## Type System

### Type Representation

```rust
/// Core type representation
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Type {
    /// Primitive types: int, boolean, etc.
    Primitive(PrimitiveType),
    
    /// Reference to a class/interface with type arguments
    Class {
        def: TypeId,
        args: Vec<Type>,
    },
    
    /// Array type
    Array(Box<Type>),
    
    /// Type variable (from generics)
    TypeVar(TypeVarId),
    
    /// Wildcard: ?, ? extends T, ? super T
    Wildcard {
        bound: WildcardBound,
    },
    
    /// Intersection type: A & B
    Intersection(Vec<Type>),
    
    /// The null type
    Null,
    
    /// Error/unknown type (for error recovery)
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum WildcardBound {
    Unbounded,
    Extends(Box<Type>),
    Super(Box<Type>),
}
```

### Subtyping

```rust
/// Check if `sub` is a subtype of `super`
#[query]
pub fn is_subtype(
    db: &dyn Database,
    sub: Type,
    super_: Type,
) -> bool {
    match (&sub, &super_) {
        // Same type is always subtype
        (a, b) if a == b => true,
        
        // null is subtype of any reference type
        (Type::Null, Type::Class { .. }) => true,
        (Type::Null, Type::Array(_)) => true,
        
        // Primitive widening
        (Type::Primitive(a), Type::Primitive(b)) => {
            primitive_widening(*a, *b)
        }
        
        // Class subtyping
        (Type::Class { def: sub_def, args: sub_args },
         Type::Class { def: super_def, args: super_args }) => {
            // Check if sub extends/implements super
            if let Some(path) = db.supertype_path(*sub_def, *super_def) {
                // Substitute type arguments along path
                let substituted = substitute_along_path(db, sub_args, path);
                // Check type argument compatibility
                type_args_compatible(db, &substituted, super_args)
            } else {
                false
            }
        }
        
        // Array covariance
        (Type::Array(sub_elem), Type::Array(super_elem)) => {
            // Arrays are covariant for reference types
            if sub_elem.is_reference() && super_elem.is_reference() {
                db.is_subtype((**sub_elem).clone(), (**super_elem).clone())
            } else {
                sub_elem == super_elem
            }
        }
        
        // Array extends Object, Cloneable, Serializable
        (Type::Array(_), Type::Class { def, .. }) => {
            db.is_object(*def) || 
            db.is_cloneable(*def) || 
            db.is_serializable(*def)
        }
        
        // Wildcard bounds
        (_, Type::Wildcard { bound: WildcardBound::Super(lower) }) => {
            db.is_subtype((**lower).clone(), sub.clone())
        }
        
        (_, Type::Wildcard { bound: WildcardBound::Extends(upper) }) => {
            db.is_subtype(sub.clone(), (**upper).clone())
        }
        
        _ => false,
    }
}
```

---

## Method Resolution

### Overload Resolution

```
┌─────────────────────────────────────────────────────────────────┐
│                    OVERLOAD RESOLUTION                           │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  PHASE 1: Identify candidate methods                            │
│  • All methods with matching name                               │
│  • In class and all superclasses/interfaces                     │
│  • Static methods from static import                            │
│                                                                  │
│  PHASE 2: Check applicability                                   │
│  For each candidate:                                            │
│  • Check arity (parameter count)                                │
│  • Check type compatibility of each argument                    │
│  • Consider varargs expansion                                   │
│                                                                  │
│  PHASE 3: Find most specific                                    │
│  Among applicable methods:                                      │
│  • Method A is more specific than B if A's params are           │
│    subtypes of B's params                                       │
│  • If unique most specific exists → select it                   │
│  • Otherwise → ambiguous (error)                                │
│                                                                  │
│  COMPLEXITY:                                                     │
│  • Generics: type argument inference                            │
│  • Varargs: special handling                                    │
│  • Boxing: int → Integer conversions                            │
│  • Lambda: target type inference                                │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Implementation

```rust
#[query]
pub fn resolve_method_call(
    db: &dyn Database,
    call: MethodCallId,
) -> MethodResolution {
    let call_data = db.method_call(call);
    let receiver_type = db.type_of(call_data.receiver);
    
    // Phase 1: Gather candidates
    let candidates = db.methods_named(&receiver_type, &call_data.name);
    
    // Phase 2: Filter applicable
    let applicable: Vec<_> = candidates
        .iter()
        .filter(|m| is_applicable(db, m, &call_data.args))
        .collect();
    
    if applicable.is_empty() {
        return MethodResolution::NotFound;
    }
    
    // Phase 3: Find most specific
    match find_most_specific(db, &applicable, &call_data.args) {
        Some(method) => MethodResolution::Found(method),
        None => MethodResolution::Ambiguous(applicable),
    }
}

fn is_applicable(
    db: &dyn Database,
    method: &Method,
    args: &[ExprId],
) -> bool {
    let params = &method.params;
    
    // Check arity
    if method.is_varargs {
        if args.len() < params.len() - 1 {
            return false;
        }
    } else {
        if args.len() != params.len() {
            return false;
        }
    }
    
    // Check each argument
    for (i, arg) in args.iter().enumerate() {
        let arg_type = db.type_of(*arg);
        let param_type = if i >= params.len() - 1 && method.is_varargs {
            // Vararg element type
            params.last().unwrap().element_type()
        } else {
            &params[i].ty
        };
        
        if !db.is_assignable(arg_type, param_type.clone()) {
            return false;
        }
    }
    
    true
}
```

---

## Type Inference

### Local Variable Type Inference (var)

```rust
#[query]
pub fn infer_var_type(
    db: &dyn Database,
    var: LocalVarId,
) -> Type {
    let var_data = db.local_var(var);
    
    match var_data.kind {
        LocalVarKind::Explicit(ty) => ty,
        
        LocalVarKind::Var => {
            // Infer from initializer
            if let Some(init) = var_data.initializer {
                db.type_of(init)
            } else {
                Type::Error // var without initializer is error
            }
        }
        
        LocalVarKind::ForEach(iterable) => {
            // Infer from iterable element type
            let iter_type = db.type_of(iterable);
            infer_foreach_element_type(db, iter_type)
        }
    }
}
```

### Lambda Type Inference

```
┌─────────────────────────────────────────────────────────────────┐
│                    LAMBDA INFERENCE                              │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  Target typing: Lambda type determined by context               │
│                                                                  │
│  Example:                                                       │
│  Function<String, Integer> f = s -> s.length();                 │
│                                                                  │
│  1. Target type is Function<String, Integer>                    │
│  2. Function is @FunctionalInterface                            │
│  3. Its SAM (Single Abstract Method) is: R apply(T)             │
│  4. With type args: Integer apply(String)                       │
│  5. Lambda parameter 's' inferred as String                     │
│  6. Lambda return type must be compatible with Integer          │
│                                                                  │
│  Complex case: method argument                                  │
│  list.stream().map(x -> x.length())                             │
│                                                                  │
│  1. map expects Function<? super T, ? extends R>                │
│  2. T is String (from stream type)                              │
│  3. x inferred as String                                        │
│  4. Return type Integer inferred from body                      │
│  5. R inferred as Integer                                       │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Generic Method Type Argument Inference

```rust
/// Infer type arguments for generic method call
#[query]
pub fn infer_type_arguments(
    db: &dyn Database,
    call: MethodCallId,
    method: MethodId,
) -> Vec<Type> {
    let method_data = db.method(method);
    let call_data = db.method_call(call);
    
    if method_data.type_params.is_empty() {
        return vec![];
    }
    
    // If explicit type arguments provided, use them
    if !call_data.type_args.is_empty() {
        return call_data.type_args.clone();
    }
    
    // Inference algorithm (simplified JLS 18)
    let mut constraints = Vec::new();
    
    // Constraints from arguments
    for (arg, param) in call_data.args.iter().zip(&method_data.params) {
        let arg_type = db.type_of(*arg);
        add_constraints(&mut constraints, &arg_type, &param.ty);
    }
    
    // Constraints from return type context
    if let Some(target) = call_data.target_type {
        add_constraints(&mut constraints, &method_data.return_type, &target);
    }
    
    // Solve constraint system
    solve_constraints(constraints, &method_data.type_params)
}
```

---

## Flow Analysis

### Definite Assignment

```
┌─────────────────────────────────────────────────────────────────┐
│                    DEFINITE ASSIGNMENT                           │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  Track which variables are definitely assigned at each point    │
│                                                                  │
│  int x;                    // x: unassigned                     │
│  if (condition) {                                               │
│    x = 1;                  // x: assigned in this branch        │
│  } else {                                                       │
│    x = 2;                  // x: assigned in this branch        │
│  }                                                              │
│  // x: definitely assigned (assigned in both branches)         │
│  use(x);                   // OK                                │
│                                                                  │
│  int y;                                                         │
│  if (condition) {                                               │
│    y = 1;                                                       │
│  }                                                              │
│  // y: NOT definitely assigned (only one branch)               │
│  use(y);                   // ERROR                             │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Null Flow Analysis

```rust
/// Track nullability through control flow
#[query]
pub fn null_state_at(
    db: &dyn Database,
    expr: ExprId,
    var: LocalVarId,
) -> NullState {
    // Build control flow graph
    let cfg = db.control_flow_graph(expr.function());
    
    // Perform dataflow analysis
    let analysis = NullFlowAnalysis::new(db);
    let states = analysis.analyze(&cfg);
    
    // Get state at expression
    states.get(&expr).and_then(|s| s.get(&var)).copied()
        .unwrap_or(NullState::Unknown)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NullState {
    /// Definitely null
    Null,
    /// Definitely not null
    NonNull,
    /// Might be null
    Unknown,
}
```

---

## Demand-Driven Analysis

### Key Innovation: On-Demand Type Checking

Traditional type checkers process entire files. Nova computes types on-demand:

```rust
/// Type-check only what's needed
#[query]
pub fn type_of(db: &dyn Database, expr: ExprId) -> Type {
    let expr_data = db.expr(expr);
    
    match &expr_data.kind {
        ExprKind::Literal(lit) => literal_type(lit),
        
        ExprKind::Variable(var) => {
            // Resolve variable, get its type
            match db.resolve_name(expr_data.scope, var.clone()) {
                Some(Resolution::Local(local)) => db.local_var_type(local),
                Some(Resolution::Field(field)) => db.field_type(field),
                Some(Resolution::Parameter(param)) => db.param_type(param),
                _ => Type::Error,
            }
        }
        
        ExprKind::MethodCall { receiver, name, args } => {
            // Only type-check receiver and args as needed
            let recv_type = db.type_of(*receiver);
            let resolution = db.resolve_method_call(expr);
            
            match resolution {
                MethodResolution::Found(method) => {
                    // Substitute type arguments
                    let type_args = db.infer_type_arguments(expr, method);
                    substitute_return_type(db, method, &type_args)
                }
                _ => Type::Error,
            }
        }
        
        ExprKind::FieldAccess { receiver, field } => {
            let recv_type = db.type_of(*receiver);
            db.resolve_field(recv_type, field)
                .map(|f| db.field_type(f))
                .unwrap_or(Type::Error)
        }
        
        // ... other expression kinds
    }
}
```

### Benefits

```
┌─────────────────────────────────────────────────────────────────┐
│                    DEMAND-DRIVEN BENEFITS                        │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  SCENARIO: User hovers over expression in 10,000 line file      │
│                                                                  │
│  TRADITIONAL APPROACH:                                          │
│  1. Parse entire file                                           │
│  2. Type-check entire file                                      │
│  3. Return type for hovered expression                          │
│  Time: 500ms+                                                   │
│                                                                  │
│  NOVA APPROACH:                                                 │
│  1. Parse file (if not cached)                                  │
│  2. Type-check ONLY hovered expression                          │
│  3. Recursively type-check dependencies (usually few)           │
│  Time: <50ms                                                    │
│                                                                  │
│  WHY IT WORKS:                                                  │
│  • Most expressions don't depend on all other expressions       │
│  • Local reasoning is usually sufficient                        │
│  • Query caching means repeated analysis is free                │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

---

## Error Recovery in Semantic Analysis

```rust
/// Semantic analysis with error recovery
impl SemanticAnalyzer {
    fn analyze_expression(&self, expr: &Expr) -> Type {
        match self.try_analyze(expr) {
            Ok(ty) => ty,
            Err(e) => {
                // Record error but continue
                self.errors.push(e);
                
                // Return best-effort type
                self.guess_type(expr)
            }
        }
    }
    
    fn guess_type(&self, expr: &Expr) -> Type {
        // Heuristics for error recovery
        match &expr.kind {
            // If method call failed, try to guess from method name
            ExprKind::MethodCall { name, .. } => {
                if name.ends_with("String") {
                    Type::string()
                } else if name.starts_with("get") || name.starts_with("is") {
                    // Likely getter, can't determine type
                    Type::Error
                } else {
                    Type::Error
                }
            }
            
            // For binary ops, infer from operator
            ExprKind::Binary { op: BinaryOp::Add, .. } => {
                // Could be numeric or string concat
                Type::Error
            }
            
            _ => Type::Error,
        }
    }
}
```

---

## Performance Considerations

```
┌─────────────────────────────────────────────────────────────────┐
│                    PERFORMANCE TARGETS                           │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  QUERY LATENCIES (cached)                                       │
│  • type_of(expr): < 1μs                                         │
│  • resolve_method_call: < 10μs                                  │
│  • is_subtype: < 1μs                                            │
│                                                                  │
│  QUERY LATENCIES (uncached, typical)                            │
│  • type_of(expr): < 1ms                                         │
│  • file diagnostics: < 50ms                                     │
│  • project-wide type check: < 10s (parallelized)                │
│                                                                  │
│  MEMORY                                                          │
│  • Type representation: 48-64 bytes                             │
│  • Symbol table per file: ~10KB                                 │
│  • Cached types per file: ~100KB                                │
│                                                                  │
│  OPTIMIZATIONS                                                   │
│  • Type interning (same type = same pointer)                    │
│  • Lazy supertype computation                                   │
│  • Parallel independent queries                                 │
│  • Early termination on errors                                  │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

---

## Next Steps

1. → [Code Intelligence](07-code-intelligence.md): How semantic analysis powers IDE features
2. → [Refactoring Engine](08-refactoring-engine.md): Safe code transformations

---

[← Previous: Syntax and Parsing](05-syntax-and-parsing.md) | [Next: Code Intelligence →](07-code-intelligence.md)
