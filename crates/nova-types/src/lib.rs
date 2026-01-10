//! Shared types and Java type system primitives for Nova.
//!
//! This crate serves two roles:
//! 1) A small "shared types" crate used across Nova crates (framework analyzers,
//!    flow analysis, classpath/JDK stubs, diagnostics, etc).
//! 2) The core of Nova's Java semantic/type understanding: a compact `Type`
//!    representation plus helper algorithms (assignability, overload resolution,
//!    and a handful of inference utilities).
//!
//! The type system implementation is intentionally best-effort (suitable for an
//! IDE) rather than a full JLS implementation.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// === Generic shared types ====================================================

/// A byte-span into a source string.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    pub fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    pub fn is_empty(&self) -> bool {
        self.start >= self.end
    }
}

impl fmt::Debug for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Span({}..{})", self.start, self.end)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Info,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: Severity,
    pub code: &'static str,
    pub message: String,
    pub span: Option<Span>,
}

impl Diagnostic {
    pub fn error(code: &'static str, message: impl Into<String>, span: Option<Span>) -> Self {
        Self {
            severity: Severity::Error,
            code,
            message: message.into(),
            span,
        }
    }

    pub fn warning(code: &'static str, message: impl Into<String>, span: Option<Span>) -> Self {
        Self {
            severity: Severity::Warning,
            code,
            message: message.into(),
            span,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionItem {
    pub label: String,
    pub detail: Option<String>,
}

impl CompletionItem {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            detail: None,
        }
    }
}

// -----------------------------------------------------------------------------
// Framework/type-checker stubs
// -----------------------------------------------------------------------------

/// A project is a build unit with its own classpath/dependencies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProjectId(pub u32);

impl ProjectId {
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }
}

/// Identifier for a Java class (top-level or nested).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ClassId(pub u32);

impl ClassId {
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TypeVarId(pub u32);

// === Type representation (core) =============================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrimitiveType {
    Boolean,
    Byte,
    Short,
    Char,
    Int,
    Long,
    Float,
    Double,
}

impl PrimitiveType {
    pub fn is_numeric(self) -> bool {
        matches!(
            self,
            PrimitiveType::Byte
                | PrimitiveType::Short
                | PrimitiveType::Char
                | PrimitiveType::Int
                | PrimitiveType::Long
                | PrimitiveType::Float
                | PrimitiveType::Double
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClassType {
    pub def: ClassId,
    pub args: Vec<Type>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum WildcardBound {
    Unbounded,
    Extends(Box<Type>),
    Super(Box<Type>),
}

/// Java type representation.
///
/// The variants are modelled after `docs/06-semantic-analysis.md` with a few
/// Nova-specific additions (`Named`, `VirtualInner`) that are used by framework
/// analyzers before the full classpath/JDK model is wired in.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Type {
    /// The special `void` type.
    Void,

    /// Primitive types: int, boolean, etc.
    Primitive(PrimitiveType),

    /// Reference to a class/interface with type arguments.
    Class(ClassType),

    /// Array type.
    Array(Box<Type>),

    /// Type variable (from generics).
    TypeVar(TypeVarId),

    /// Wildcard: ?, ? extends T, ? super T
    Wildcard(WildcardBound),

    /// Intersection type: A & B
    Intersection(Vec<Type>),

    /// The null type.
    Null,

    /// Refers to a class not tracked by the database (e.g. external libraries).
    ///
    /// This uses the Java binary name (`java.lang.String`).
    Named(String),

    /// Virtual inner class produced by a framework analyzer.
    VirtualInner { owner: ClassId, name: String },

    /// An unknown type (e.g. missing symbol). Used for error recovery.
    Unknown,

    /// An error type (e.g. type mismatch). Used for error recovery.
    Error,
}

impl Type {
    pub fn class(def: ClassId, args: Vec<Type>) -> Self {
        Type::Class(ClassType { def, args })
    }

    pub fn boolean() -> Self {
        Type::Primitive(PrimitiveType::Boolean)
    }

    pub fn int() -> Self {
        Type::Primitive(PrimitiveType::Int)
    }

    pub fn is_primitive_boolean(&self) -> bool {
        matches!(self, Type::Primitive(PrimitiveType::Boolean))
    }

    pub fn is_reference(&self) -> bool {
        matches!(
            self,
            Type::Class(_)
                | Type::Array(_)
                | Type::TypeVar(_)
                | Type::Intersection(_)
                | Type::Named(_)
                | Type::VirtualInner { .. }
        )
    }

    pub fn is_errorish(&self) -> bool {
        matches!(self, Type::Unknown | Type::Error)
    }

    pub fn array_element(&self) -> Option<&Type> {
        match self {
            Type::Array(elem) => Some(elem.as_ref()),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parameter {
    pub name: String,
    pub ty: Type,
}

impl Parameter {
    pub fn new(name: impl Into<String>, ty: Type) -> Self {
        Self {
            name: name.into(),
            ty,
        }
    }
}
// --- External type stubs -----------------------------------------------------
//
// Nova's early semantic layers need a way to reason about types that come from
// compiled dependencies (jars, output directories, etc). Full type-checking will
// eventually use a richer model; these stubs are a lightweight bridge.

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldStub {
    pub name: String,
    /// Field descriptor, e.g. `Ljava/lang/String;`.
    pub descriptor: String,
    pub signature: Option<String>,
    pub access_flags: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MethodStub {
    pub name: String,
    /// Method descriptor, e.g. `(I)Ljava/lang/String;`.
    pub descriptor: String,
    pub signature: Option<String>,
    pub access_flags: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemberStub {
    Field(FieldStub),
    Method(MethodStub),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeDefStub {
    pub binary_name: String,
    pub access_flags: u16,
    pub super_binary_name: Option<String>,
    pub interfaces: Vec<String>,
    pub signature: Option<String>,
    pub fields: Vec<FieldStub>,
    pub methods: Vec<MethodStub>,
}

/// A source of types used by the semantic layers.
///
/// Implementations can be backed by the JDK, a project index, third-party jars, etc.
pub trait TypeProvider {
    fn lookup_type(&self, binary_name: &str) -> Option<TypeDefStub>;

    fn members(&self, binary_name: &str) -> Vec<MemberStub> {
        let Some(ty) = self.lookup_type(binary_name) else {
            return Vec::new();
        };
        ty.fields
            .into_iter()
            .map(MemberStub::Field)
            .chain(ty.methods.into_iter().map(MemberStub::Method))
            .collect()
    }

    fn supertypes(&self, binary_name: &str) -> Vec<String> {
        let Some(ty) = self.lookup_type(binary_name) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if let Some(super_name) = ty.super_binary_name {
            out.push(super_name);
        }
        out.extend(ty.interfaces);
        out
    }
}

/// The semantic layers often want to consult multiple sources (project deps, JDK, etc.). A simple
/// `TypeProvider` implementation that tries each provider in order.
pub struct ChainTypeProvider<'a> {
    providers: Vec<&'a dyn TypeProvider>,
}

impl<'a> ChainTypeProvider<'a> {
    pub fn new(providers: Vec<&'a dyn TypeProvider>) -> Self {
        Self { providers }
    }
}

impl<'a> TypeProvider for ChainTypeProvider<'a> {
    fn lookup_type(&self, binary_name: &str) -> Option<TypeDefStub> {
        self.providers
            .iter()
            .find_map(|p| p.lookup_type(binary_name))
    }

    fn members(&self, binary_name: &str) -> Vec<MemberStub> {
        self.providers
            .iter()
            .find_map(|p| {
                let m = p.members(binary_name);
                if m.is_empty() { None } else { Some(m) }
            })
            .unwrap_or_default()
    }

    fn supertypes(&self, binary_name: &str) -> Vec<String> {
        self.providers
            .iter()
            .find_map(|p| {
                let s = p.supertypes(binary_name);
                if s.is_empty() { None } else { Some(s) }
            })
            .unwrap_or_default()
    }
}

/// A `TypeProvider` that always reports types as missing.
pub struct EmptyTypeProvider;

impl TypeProvider for EmptyTypeProvider {
    fn lookup_type(&self, _binary_name: &str) -> Option<TypeDefStub> {
        None
    }
}

// === Java type environment (nova-types) ======================================

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ClassKind {
    Class,
    Interface,
}

#[derive(Debug, Clone)]
pub struct TypeParamDef {
    pub name: String,
    pub upper_bounds: Vec<Type>,
    /// Capture conversion may introduce a lower bound (`? super T`).
    pub lower_bound: Option<Type>,
}

#[derive(Debug, Clone)]
pub struct MethodDef {
    pub name: String,
    pub type_params: Vec<TypeVarId>,
    pub params: Vec<Type>,
    pub return_type: Type,
    pub is_static: bool,
    pub is_varargs: bool,
    pub is_abstract: bool,
}

impl MethodDef {
    pub fn param_types_for_arity(&self, arity: usize) -> Vec<Type> {
        if !self.is_varargs {
            return self.params.clone();
        }

        if self.params.is_empty() {
            return vec![];
        }

        let fixed = self.params.len() - 1;
        let mut out = Vec::with_capacity(arity.max(self.params.len()));
        out.extend(self.params[..fixed].iter().cloned());

        let vararg_ty = self.params[fixed].clone();
        let elem_ty = match vararg_ty {
            Type::Array(elem) => *elem,
            other => other,
        };
        let extra = arity.saturating_sub(fixed);
        for _ in 0..extra {
            out.push(elem_ty.clone());
        }

        out
    }
}

#[derive(Debug, Clone)]
pub struct ClassDef {
    pub name: String,
    pub kind: ClassKind,
    pub type_params: Vec<TypeVarId>,
    pub super_class: Option<Type>,
    pub interfaces: Vec<Type>,
    pub methods: Vec<MethodDef>,
}

#[derive(Debug, Clone)]
pub struct WellKnownTypes {
    pub object: ClassId,
    pub string: ClassId,
    pub integer: ClassId,
    pub cloneable: ClassId,
    pub serializable: ClassId,
}

pub trait TypeEnv {
    fn class(&self, id: ClassId) -> Option<&ClassDef>;
    fn type_param(&self, id: TypeVarId) -> Option<&TypeParamDef>;
    fn lookup_class(&self, name: &str) -> Option<ClassId>;
    fn well_known(&self) -> &WellKnownTypes;
}

/// Hook for adding project / classpath types.
///
/// The production implementation will likely load class files (or stubs) from
/// the user's classpath and feed them into a `TypeStore`. For now it's
/// deliberately minimal.
pub trait ClasspathTypes {
    fn classes(&self) -> Vec<ClassDef> {
        Vec::new()
    }
}

impl ClasspathTypes for () {}

#[derive(Debug, Default)]
pub struct TypeStore {
    classes: Vec<ClassDef>,
    class_by_name: HashMap<String, ClassId>,
    type_params: Vec<TypeParamDef>,
    well_known: Option<WellKnownTypes>,
}

impl TypeStore {
    pub fn with_minimal_jdk() -> Self {
        let mut store = TypeStore::default();

        // java.lang
        let object = store.add_class(ClassDef {
            name: "java.lang.Object".to_string(),
            kind: ClassKind::Class,
            type_params: vec![],
            super_class: None,
            interfaces: vec![],
            methods: vec![],
        });
        let string = store.add_class(ClassDef {
            name: "java.lang.String".to_string(),
            kind: ClassKind::Class,
            type_params: vec![],
            super_class: Some(Type::class(object, vec![])),
            interfaces: vec![],
            methods: vec![],
        });
        let integer = store.add_class(ClassDef {
            name: "java.lang.Integer".to_string(),
            kind: ClassKind::Class,
            type_params: vec![],
            super_class: Some(Type::class(object, vec![])),
            interfaces: vec![],
            methods: vec![],
        });
        let cloneable = store.add_class(ClassDef {
            name: "java.lang.Cloneable".to_string(),
            kind: ClassKind::Interface,
            type_params: vec![],
            super_class: None,
            interfaces: vec![],
            methods: vec![],
        });
        let serializable = store.add_class(ClassDef {
            name: "java.io.Serializable".to_string(),
            kind: ClassKind::Interface,
            type_params: vec![],
            super_class: None,
            interfaces: vec![],
            methods: vec![],
        });

        // java.util.List<E>
        let list_e = store.add_type_param("E", vec![Type::class(object, vec![])]);
        let list = store.add_class(ClassDef {
            name: "java.util.List".to_string(),
            kind: ClassKind::Interface,
            type_params: vec![list_e],
            super_class: None,
            interfaces: vec![],
            methods: vec![
                MethodDef {
                    name: "get".to_string(),
                    type_params: vec![],
                    params: vec![Type::Primitive(PrimitiveType::Int)],
                    return_type: Type::TypeVar(list_e),
                    is_static: false,
                    is_varargs: false,
                    is_abstract: true,
                },
                MethodDef {
                    name: "add".to_string(),
                    type_params: vec![],
                    params: vec![Type::TypeVar(list_e)],
                    return_type: Type::Primitive(PrimitiveType::Boolean),
                    is_static: false,
                    is_varargs: false,
                    is_abstract: true,
                },
            ],
        });

        // java.util.ArrayList<E> implements List<E>
        let array_list_e = store.add_type_param("E", vec![Type::class(object, vec![])]);
        let _array_list = store.add_class(ClassDef {
            name: "java.util.ArrayList".to_string(),
            kind: ClassKind::Class,
            type_params: vec![array_list_e],
            super_class: Some(Type::class(object, vec![])),
            interfaces: vec![Type::class(list, vec![Type::TypeVar(array_list_e)])],
            methods: vec![],
        });

        // java.util.function.Function<T, R>
        let function_t = store.add_type_param("T", vec![Type::class(object, vec![])]);
        let function_r = store.add_type_param("R", vec![Type::class(object, vec![])]);
        let _function = store.add_class(ClassDef {
            name: "java.util.function.Function".to_string(),
            kind: ClassKind::Interface,
            type_params: vec![function_t, function_r],
            super_class: None,
            interfaces: vec![],
            methods: vec![MethodDef {
                name: "apply".to_string(),
                type_params: vec![],
                params: vec![Type::TypeVar(function_t)],
                return_type: Type::TypeVar(function_r),
                is_static: false,
                is_varargs: false,
                is_abstract: true,
            }],
        });

        store.well_known = Some(WellKnownTypes {
            object,
            string,
            integer,
            cloneable,
            serializable,
        });

        store
    }

    pub fn with_minimal_jdk_and_classpath(classpath: &dyn ClasspathTypes) -> Self {
        let mut store = TypeStore::with_minimal_jdk();
        for class in classpath.classes() {
            store.add_class(class);
        }
        store
    }

    pub fn add_type_param(&mut self, name: impl Into<String>, upper_bounds: Vec<Type>) -> TypeVarId {
        let id = TypeVarId(self.type_params.len() as u32);
        self.type_params.push(TypeParamDef {
            name: name.into(),
            upper_bounds,
            lower_bound: None,
        });
        id
    }

    fn add_capture_type_param(&mut self, upper_bounds: Vec<Type>, lower_bound: Option<Type>) -> TypeVarId {
        let id = TypeVarId(self.type_params.len() as u32);
        self.type_params.push(TypeParamDef {
            name: format!("CAP#{}", id.0),
            upper_bounds,
            lower_bound,
        });
        id
    }

    /// Capture conversion for parameterized types containing wildcards (JLS 5.1.10).
    ///
    /// This is a best-effort implementation intended for common IDE scenarios.
    /// It allocates fresh `TypeVarId`s inside the store to represent capture
    /// variables.
    pub fn capture_conversion(&mut self, ty: &Type) -> Type {
        let Type::Class(ClassType { def, args }) = ty else {
            return ty.clone();
        };

        if args.iter().all(|a| !matches!(a, Type::Wildcard(_))) {
            return ty.clone();
        }

        let Some(class_def) = self.class(*def) else {
            return ty.clone();
        };

        let object = Type::class(self.well_known().object, vec![]);
        let formal_bounds: Vec<Type> = class_def
            .type_params
            .iter()
            .map(|tp| {
                self.type_param(*tp)
                    .and_then(|d| d.upper_bounds.first().cloned())
                    .unwrap_or_else(|| object.clone())
            })
            .collect();

        let mut new_args = Vec::with_capacity(args.len());
        for (idx, arg) in args.iter().enumerate() {
            match arg {
                Type::Wildcard(WildcardBound::Unbounded) => {
                    let upper = formal_bounds.get(idx).cloned().unwrap_or_else(|| object.clone());
                    let cap = self.add_capture_type_param(vec![upper], None);
                    new_args.push(Type::TypeVar(cap));
                }
                Type::Wildcard(WildcardBound::Extends(upper)) => {
                    let formal = formal_bounds.get(idx).cloned().unwrap_or_else(|| object.clone());
                    let glb = glb(self, &formal, upper);
                    let cap = self.add_capture_type_param(vec![glb], None);
                    new_args.push(Type::TypeVar(cap));
                }
                Type::Wildcard(WildcardBound::Super(lower)) => {
                    let upper = formal_bounds.get(idx).cloned().unwrap_or_else(|| object.clone());
                    let cap = self.add_capture_type_param(vec![upper], Some((**lower).clone()));
                    new_args.push(Type::TypeVar(cap));
                }
                other => new_args.push(other.clone()),
            }
        }

        Type::class(*def, new_args)
    }

    pub fn add_class(&mut self, mut def: ClassDef) -> ClassId {
        let id = ClassId(self.classes.len() as u32);
        if self.class_by_name.contains_key(&def.name) {
            // Avoid silently creating two ids for the same class.
            // This is a programmer error in tests/builders.
            panic!("duplicate class definition for {}", def.name);
        }
        self.class_by_name.insert(def.name.clone(), id);
        if def.methods.is_empty() {
            def.methods = Vec::new();
        }
        self.classes.push(def);
        id
    }

    pub fn class_id(&self, name: &str) -> Option<ClassId> {
        self.lookup_class(name)
    }
}

impl TypeEnv for TypeStore {
    fn class(&self, id: ClassId) -> Option<&ClassDef> {
        self.classes.get(id.0 as usize)
    }

    fn type_param(&self, id: TypeVarId) -> Option<&TypeParamDef> {
        self.type_params.get(id.0 as usize)
    }

    fn lookup_class(&self, name: &str) -> Option<ClassId> {
        if let Some(id) = self.class_by_name.get(name).copied() {
            return Some(id);
        }

        // Best-effort support for the implicit `java.lang.*` universe scope.
        // This mirrors Java name resolution rules where `java.lang` is imported
        // automatically, but avoids forcing callers to always use fully-qualified
        // names for common types like `String`.
        if !name.contains('.') {
            let jlang = format!("java.lang.{name}");
            return self.class_by_name.get(&jlang).copied();
        }

        None
    }

fn well_known(&self) -> &WellKnownTypes {
        self.well_known
            .as_ref()
            .expect("TypeStore::with_minimal_jdk must initialize well-known types")
    }
}

// === Subtyping / assignability ==============================================

pub fn is_subtype(env: &dyn TypeEnv, sub: &Type, super_: &Type) -> bool {
    if sub == super_ {
        return true;
    }

    // Resolve `Type::Named("java.lang.String")` into a known JDK class type when possible.
    if let Type::Named(name) = sub {
        if let Some(id) = env.lookup_class(name) {
            return is_subtype(env, &Type::class(id, vec![]), super_);
        }
    }
    if let Type::Named(name) = super_ {
        if let Some(id) = env.lookup_class(name) {
            return is_subtype(env, sub, &Type::class(id, vec![]));
        }
    }

    // Error recovery: unknown/error is treated as compatible with everything.
    if sub.is_errorish() || super_.is_errorish() {
        return true;
    }

    match (sub, super_) {
        // `void` is only compatible with itself (handled by equality above).
        (Type::Void, _) | (_, Type::Void) => false,

        // null is subtype of any reference type
        (Type::Null, t) if t.is_reference() || matches!(t, Type::Wildcard(_)) => true,

        (Type::Primitive(a), Type::Primitive(b)) => primitive_widening(*a, *b),

        (Type::Array(sub_elem), Type::Array(super_elem)) => {
            if sub_elem.is_reference() && super_elem.is_reference() {
                is_subtype(env, sub_elem, super_elem)
            } else {
                sub_elem == super_elem
            }
        }

        // Arrays extend Object, Cloneable, Serializable
        (Type::Array(_), Type::Class(ClassType { def, .. })) => {
            let wk = env.well_known();
            *def == wk.object || *def == wk.cloneable || *def == wk.serializable
        }

        (Type::Intersection(types), other) => types.iter().any(|t| is_subtype(env, t, other)),

        (other, Type::Intersection(types)) => types.iter().all(|t| is_subtype(env, other, t)),

        (Type::TypeVar(id), other) => env
            .type_param(*id)
            .map(|tp| {
                if tp.upper_bounds.is_empty() {
                    false
                } else {
                    tp.upper_bounds.iter().any(|b| is_subtype(env, b, other))
                }
            })
            .unwrap_or(false),

        (other, Type::TypeVar(id)) => {
            env.type_param(*id)
                .map(|tp| {
                    if let Some(lower) = &tp.lower_bound {
                        is_subtype(env, other, lower)
                    } else {
                        // For declared type variables without a lower bound we
                        // can't generally decide `other <: T` (it depends on
                        // the eventual instantiation), so be conservative.
                        false
                    }
                })
                .unwrap_or(false)
        }

        (_, Type::Wildcard(WildcardBound::Unbounded)) => true,
        (_, Type::Wildcard(WildcardBound::Extends(upper))) => is_subtype(env, sub, upper),
        (_, Type::Wildcard(WildcardBound::Super(lower))) => is_subtype(env, lower, sub),

        // Best-effort: treat framework-only synthetic types as subtypes of Object.
        (Type::VirtualInner { .. } | Type::Named(_), Type::Class(ClassType { def, .. })) => {
            *def == env.well_known().object
        }

        (Type::Class(_), Type::Class(_)) => is_subtype_class(env, sub, super_),

        _ => false,
    }
}

fn primitive_widening(from: PrimitiveType, to: PrimitiveType) -> bool {
    use PrimitiveType::*;
    if from == to {
        return true;
    }
    match (from, to) {
        (Byte, Short | Int | Long | Float | Double) => true,
        (Short, Int | Long | Float | Double) => true,
        (Char, Int | Long | Float | Double) => true,
        (Int, Long | Float | Double) => true,
        (Long, Float | Double) => true,
        (Float, Double) => true,
        _ => false,
    }
}

fn is_subtype_class(env: &dyn TypeEnv, sub: &Type, super_: &Type) -> bool {
    let (sub_def, sub_args) = match sub {
        Type::Class(ClassType { def, args }) => (*def, args.clone()),
        _ => return false,
    };
    let (super_def, super_args) = match super_ {
        Type::Class(ClassType { def, args }) => (*def, args.clone()),
        _ => return false,
    };

    let mut queue = VecDeque::new();
    let mut seen = HashSet::new();
    queue.push_back(Type::class(sub_def, sub_args));

    while let Some(current) = queue.pop_front() {
        let Type::Class(ClassType { def, args }) = current.clone() else {
            continue;
        };
        if !seen.insert((def, args.clone())) {
            continue;
        }

        if def == super_def {
            return type_args_compatible(env, def, &args, &super_args);
        }

        let Some(class_def) = env.class(def) else {
            continue;
        };

        let subst = class_def
            .type_params
            .iter()
            .copied()
            .zip(args.into_iter())
            .collect::<HashMap<_, _>>();

        if let Some(sc) = &class_def.super_class {
            queue.push_back(substitute(sc, &subst));
        }
        for iface in &class_def.interfaces {
            queue.push_back(substitute(iface, &subst));
        }
    }

    false
}

fn type_args_compatible(env: &dyn TypeEnv, def: ClassId, sub: &[Type], super_: &[Type]) -> bool {
    let type_param_len = env
        .class(def)
        .map(|c| c.type_params.len())
        .unwrap_or(0);
    let sub_raw = sub.is_empty() && type_param_len != 0;
    let super_raw = super_.is_empty() && type_param_len != 0;

    // Raw target types behave like erasure: any instantiation is a subtype of
    // the raw form. (Raw -> parameterized is handled via unchecked conversion.)
    if super_raw {
        return true;
    }
    if sub_raw {
        return false;
    }

    if sub.len() != super_.len() {
        return false;
    }
    for (s, t) in sub.iter().zip(super_) {
        match t {
            Type::Wildcard(WildcardBound::Unbounded) => continue,
            Type::Wildcard(WildcardBound::Extends(upper)) => {
                if !is_subtype(env, s, upper) {
                    return false;
                }
            }
            Type::Wildcard(WildcardBound::Super(lower)) => {
                if !is_subtype(env, lower, s) {
                    return false;
                }
            }
            _ => {
                if s != t {
                    return false;
                }
            }
        }
    }
    true
}

fn substitute(ty: &Type, subst: &HashMap<TypeVarId, Type>) -> Type {
    match ty {
        Type::TypeVar(id) => subst.get(id).cloned().unwrap_or(Type::TypeVar(*id)),
        Type::Array(elem) => Type::Array(Box::new(substitute(elem, subst))),
        Type::Class(ClassType { def, args }) => Type::class(
            *def,
            args.iter().map(|a| substitute(a, subst)).collect(),
        ),
        Type::Wildcard(WildcardBound::Unbounded) => Type::Wildcard(WildcardBound::Unbounded),
        Type::Wildcard(WildcardBound::Extends(upper)) => Type::Wildcard(WildcardBound::Extends(
            Box::new(substitute(upper, subst)),
        )),
        Type::Wildcard(WildcardBound::Super(lower)) => Type::Wildcard(WildcardBound::Super(
            Box::new(substitute(lower, subst)),
        )),
        Type::Intersection(types) => {
            Type::Intersection(types.iter().map(|t| substitute(t, subst)).collect())
        }
        other => other.clone(),
    }
}

pub fn is_assignable(env: &dyn TypeEnv, from: &Type, to: &Type) -> bool {
    assignment_conversion(env, from, to).is_some()
}

// === Conversions (JLS 5) =====================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UncheckedReason {
    RawConversion,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeWarning {
    Unchecked(UncheckedReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConversionStep {
    Identity,
    WideningPrimitive,
    NarrowingPrimitive,
    WideningReference,
    NarrowingReference,
    Boxing,
    Unboxing,
    Unchecked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conversion {
    pub steps: Vec<ConversionStep>,
    pub warnings: Vec<TypeWarning>,
}

impl Conversion {
    fn new(step: ConversionStep) -> Self {
        Self {
            steps: vec![step],
            warnings: Vec::new(),
        }
    }

    fn push_step(mut self, step: ConversionStep) -> Self {
        self.steps.push(step);
        self
    }

    fn push_warning(mut self, warning: TypeWarning) -> Self {
        self.warnings.push(warning);
        self
    }
}

/// Unary numeric promotion (JLS 5.6.1).
pub fn unary_numeric_promotion(from: PrimitiveType) -> Option<PrimitiveType> {
    use PrimitiveType::*;
    Some(match from {
        Byte | Short | Char => Int,
        Int | Long | Float | Double => from,
        Boolean => return None,
    })
}

/// Binary numeric promotion (JLS 5.6.2).
pub fn binary_numeric_promotion(a: PrimitiveType, b: PrimitiveType) -> Option<PrimitiveType> {
    use PrimitiveType::*;
    if !a.is_numeric() || !b.is_numeric() {
        return None;
    }
    Some(if a == Double || b == Double {
        Double
    } else if a == Float || b == Float {
        Float
    } else if a == Long || b == Long {
        Long
    } else {
        Int
    })
}

fn primitive_narrowing(from: PrimitiveType, to: PrimitiveType) -> bool {
    if from == to {
        return true;
    }
    from.is_numeric() && to.is_numeric()
}

/// Strict method invocation conversion (JLS 15.12.2.2): identity, widening
/// primitive, widening reference.
pub fn strict_method_invocation_conversion(
    env: &dyn TypeEnv,
    from: &Type,
    to: &Type,
) -> Option<Conversion> {
    let from = canonicalize_named(env, from);
    let to = canonicalize_named(env, to);

    if from == to {
        return Some(Conversion::new(ConversionStep::Identity));
    }

    match (&from, &to) {
        (Type::Null, t) if t.is_reference() || matches!(t, Type::Wildcard(_)) => {
            Some(Conversion::new(ConversionStep::WideningReference))
        }
        (Type::Primitive(a), Type::Primitive(b)) if primitive_widening(*a, *b) => {
            Some(Conversion::new(ConversionStep::WideningPrimitive))
        }
        (a, b) if a.is_reference() && b.is_reference() && is_subtype(env, a, b) => {
            let mut conv = Conversion::new(ConversionStep::WideningReference);
            if raw_warning(env, a, b) {
                conv.warnings.push(TypeWarning::Unchecked(UncheckedReason::RawConversion));
            }
            Some(conv)
        }
        _ => None,
    }
}

/// Method invocation conversion (JLS 5.3): strict conversion plus boxing,
/// unboxing, and unchecked raw conversions.
pub fn method_invocation_conversion(env: &dyn TypeEnv, from: &Type, to: &Type) -> Option<Conversion> {
    let from = canonicalize_named(env, from);
    let to = canonicalize_named(env, to);

    if let Some(conv) = strict_method_invocation_conversion(env, &from, &to) {
        return Some(conv);
    }

    // Boxing (and possible widening reference after boxing).
    if let Type::Primitive(p) = from {
        if let Some(boxed) = boxing_type(env, p) {
            if boxed == to {
                return Some(Conversion::new(ConversionStep::Boxing));
            }
            if boxed.is_reference() && to.is_reference() && is_subtype(env, &boxed, &to) {
                let mut conv =
                    Conversion::new(ConversionStep::Boxing).push_step(ConversionStep::WideningReference);
                if raw_warning(env, &boxed, &to) {
                    conv.warnings.push(TypeWarning::Unchecked(UncheckedReason::RawConversion));
                }
                return Some(conv);
            }
        }
    }

    // Unboxing (and possible widening primitive after unboxing).
    if let Some(unboxed) = unbox(env, &from) {
        if let Type::Primitive(target) = to {
            if unboxed == target {
                return Some(Conversion::new(ConversionStep::Unboxing));
            }
            if primitive_widening(unboxed, target) {
                return Some(
                    Conversion::new(ConversionStep::Unboxing).push_step(ConversionStep::WideningPrimitive),
                );
            }
        }
    }

    // Unchecked conversion involving raw types.
    if let Some(conv) = unchecked_raw_conversion(env, &from, &to) {
        return Some(conv);
    }

    None
}

/// Assignment conversion (JLS 5.2).
pub fn assignment_conversion(env: &dyn TypeEnv, from: &Type, to: &Type) -> Option<Conversion> {
    method_invocation_conversion(env, from, to)
}

/// Casting conversion (JLS 5.5), implemented for common cases.
pub fn cast_conversion(env: &dyn TypeEnv, from: &Type, to: &Type) -> Option<Conversion> {
    let from = canonicalize_named(env, from);
    let to = canonicalize_named(env, to);

    if let Some(conv) = assignment_conversion(env, &from, &to) {
        return Some(conv);
    }

    // Primitive casts: allow numeric narrowing.
    if let (Type::Primitive(a), Type::Primitive(b)) = (&from, &to) {
        if primitive_narrowing(*a, *b) {
            return Some(Conversion::new(ConversionStep::NarrowingPrimitive));
        }
        return None;
    }

    // Unboxing followed by primitive cast.
    if let Some(unboxed) = unbox(env, &from) {
        if let Type::Primitive(target) = to {
            if primitive_narrowing(unboxed, target) {
                return Some(
                    Conversion::new(ConversionStep::Unboxing).push_step(ConversionStep::NarrowingPrimitive),
                );
            }
        }
    }

    // Reference casts.
    if from.is_reference() && to.is_reference() && reference_castable(env, &from, &to) {
        let mut conv = Conversion::new(ConversionStep::NarrowingReference);
        if raw_warning(env, &from, &to) {
            conv.warnings.push(TypeWarning::Unchecked(UncheckedReason::RawConversion));
        }
        return Some(conv);
    }

    // Intersection casts: `(A & B) expr` is valid iff `expr` is castable to each component.
    if let Type::Intersection(parts) = &to {
        let conv = Conversion::new(ConversionStep::NarrowingReference);
        for p in parts {
            cast_conversion(env, &from, p)?;
        }
        return Some(conv);
    }

    None
}

fn canonicalize_named(env: &dyn TypeEnv, ty: &Type) -> Type {
    match ty {
        Type::Named(name) => env
            .lookup_class(name)
            .map(|id| Type::class(id, vec![]))
            .unwrap_or_else(|| ty.clone()),
        other => other.clone(),
    }
}

fn boxing_type(env: &dyn TypeEnv, prim: PrimitiveType) -> Option<Type> {
    let name = match prim {
        PrimitiveType::Boolean => "java.lang.Boolean",
        PrimitiveType::Byte => "java.lang.Byte",
        PrimitiveType::Short => "java.lang.Short",
        PrimitiveType::Char => "java.lang.Character",
        PrimitiveType::Int => "java.lang.Integer",
        PrimitiveType::Long => "java.lang.Long",
        PrimitiveType::Float => "java.lang.Float",
        PrimitiveType::Double => "java.lang.Double",
    };
    env.lookup_class(name).map(|id| Type::class(id, vec![]))
}

fn unbox(env: &dyn TypeEnv, from: &Type) -> Option<PrimitiveType> {
    match from {
        Type::Class(ClassType { def, .. }) => env.class(*def).and_then(|c| unbox_class_name(&c.name)),
        Type::TypeVar(id) => env
            .type_param(*id)
            .and_then(|tp| tp.upper_bounds.first())
            .and_then(|b| unbox(env, b)),
        _ => None,
    }
}

fn unbox_class_name(name: &str) -> Option<PrimitiveType> {
    Some(match name {
        "java.lang.Boolean" => PrimitiveType::Boolean,
        "java.lang.Byte" => PrimitiveType::Byte,
        "java.lang.Short" => PrimitiveType::Short,
        "java.lang.Character" => PrimitiveType::Char,
        "java.lang.Integer" => PrimitiveType::Int,
        "java.lang.Long" => PrimitiveType::Long,
        "java.lang.Float" => PrimitiveType::Float,
        "java.lang.Double" => PrimitiveType::Double,
        _ => return None,
    })
}

fn is_raw_class(env: &dyn TypeEnv, def: ClassId, args: &[Type]) -> bool {
    args.is_empty()
        && env
            .class(def)
            .is_some_and(|c| !c.type_params.is_empty())
}

fn raw_warning(env: &dyn TypeEnv, from: &Type, to: &Type) -> bool {
    let (Type::Class(ClassType { def: f_def, args: f_args }), Type::Class(ClassType { def: t_def, args: t_args })) =
        (from, to)
    else {
        return false;
    };
    let from_raw = is_raw_class(env, *f_def, f_args);
    let to_raw = is_raw_class(env, *t_def, t_args);
    let from_param = !from_raw && !f_args.is_empty();
    let to_param = !to_raw && !t_args.is_empty();
    (from_raw && to_param) || (to_raw && from_param)
}

fn unchecked_raw_conversion(env: &dyn TypeEnv, from: &Type, to: &Type) -> Option<Conversion> {
    let (Type::Class(ClassType { def: f_def, args: f_args }), Type::Class(ClassType { def: t_def, args: t_args })) =
        (from, to)
    else {
        return None;
    };

    let from_raw = is_raw_class(env, *f_def, f_args);
    let to_raw = is_raw_class(env, *t_def, t_args);

    if from_raw && !to_raw && !t_args.is_empty() {
        let from_er = erasure(env, from);
        let to_er = erasure(env, to);
        if is_subtype(env, &from_er, &to_er) {
            return Some(
                Conversion::new(ConversionStep::Unchecked)
                    .push_warning(TypeWarning::Unchecked(UncheckedReason::RawConversion)),
            );
        }
    }

    // Parameterized -> raw: prefer strict widening but still surface a warning.
    if !from_raw && !f_args.is_empty() && to_raw && is_subtype(env, from, to) {
        return Some(
            Conversion::new(ConversionStep::WideningReference)
                .push_warning(TypeWarning::Unchecked(UncheckedReason::RawConversion)),
        );
    }

    None
}

fn erasure(env: &dyn TypeEnv, ty: &Type) -> Type {
    match ty {
        Type::Class(ClassType { def, .. }) => Type::class(*def, vec![]),
        Type::Array(elem) => Type::Array(Box::new(erasure(env, elem))),
        Type::TypeVar(id) => env
            .type_param(*id)
            .and_then(|tp| tp.upper_bounds.first().cloned())
            .map(|b| erasure(env, &b))
            .unwrap_or_else(|| Type::class(env.well_known().object, vec![])),
        Type::Intersection(types) => types
            .first()
            .map(|t| erasure(env, t))
            .unwrap_or_else(|| Type::class(env.well_known().object, vec![])),
        Type::Wildcard(_) => Type::class(env.well_known().object, vec![]),
        Type::Named(name) => env
            .lookup_class(name)
            .map(|id| Type::class(id, vec![]))
            .unwrap_or_else(|| Type::class(env.well_known().object, vec![])),
        other => other.clone(),
    }
}

fn reference_castable(env: &dyn TypeEnv, from: &Type, to: &Type) -> bool {
    if matches!(from, Type::Null) {
        return to.is_reference();
    }
    if is_subtype(env, from, to) || is_subtype(env, to, from) {
        return true;
    }

    // Best-effort: allow casts involving interfaces.
    let (Type::Class(ClassType { def: from_def, .. }), Type::Class(ClassType { def: to_def, .. })) = (from, to)
    else {
        return false;
    };
    let from_kind = env.class(*from_def).map(|c| c.kind);
    let to_kind = env.class(*to_def).map(|c| c.kind);
    matches!(from_kind, Some(ClassKind::Interface)) || matches!(to_kind, Some(ClassKind::Interface))
}

fn glb(env: &dyn TypeEnv, a: &Type, b: &Type) -> Type {
    if is_subtype(env, a, b) {
        return a.clone();
    }
    if is_subtype(env, b, a) {
        return b.clone();
    }
    Type::Intersection(vec![a.clone(), b.clone()])
}

// === Method resolution =======================================================

#[derive(Debug, Clone)]
pub struct MethodCall<'a> {
    pub receiver: Type,
    pub name: &'a str,
    pub args: Vec<Type>,
    pub expected_return: Option<Type>,
    pub explicit_type_args: Vec<Type>,
}

#[derive(Debug, Clone)]
pub struct ResolvedMethod {
    pub owner: ClassId,
    pub name: String,
    pub params: Vec<Type>,
    pub return_type: Type,
    pub is_varargs: bool,
    pub inferred_type_args: Vec<Type>,
    pub warnings: Vec<TypeWarning>,
    pub used_varargs: bool,
}

#[derive(Debug, Clone)]
pub enum MethodResolution {
    Found(ResolvedMethod),
    NotFound,
    Ambiguous(Vec<ResolvedMethod>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MethodPhase {
    Strict,
    Loose,
    Varargs,
}

pub fn resolve_method_call(env: &mut TypeStore, call: &MethodCall<'_>) -> MethodResolution {
    let mut receiver = call.receiver.clone();
    if let Type::Named(name) = &receiver {
        if let Some(id) = env.lookup_class(name) {
            receiver = Type::class(id, vec![]);
        }
    }
    receiver = env.capture_conversion(&receiver);

    let env_ro: &TypeStore = &*env;
    let candidates = collect_method_candidates(env_ro, &receiver, call.name);

    if candidates.is_empty() {
        return MethodResolution::NotFound;
    }

    for phase in [MethodPhase::Strict, MethodPhase::Loose, MethodPhase::Varargs] {
        let applicable: Vec<ResolvedMethod> = candidates
            .iter()
            .filter_map(|cand| check_applicability(env_ro, cand, call, phase))
            .collect();

        if applicable.is_empty() {
            continue;
        }

        return match most_specific(env_ro, &applicable, call.args.len()) {
            Some(best) => MethodResolution::Found(best.clone()),
            None => MethodResolution::Ambiguous(applicable),
        };
    }

    MethodResolution::NotFound
}

#[derive(Debug, Clone)]
struct CandidateMethod {
    owner: ClassId,
    method: MethodDef,
    class_subst: HashMap<TypeVarId, Type>,
}

fn collect_method_candidates(env: &dyn TypeEnv, receiver: &Type, name: &str) -> Vec<CandidateMethod> {
    let mut out = Vec::new();

    let start = match receiver {
        Type::Class(_) => receiver.clone(),
        Type::Array(_) => Type::class(env.well_known().object, vec![]),
        _ => return out,
    };

    let mut queue = VecDeque::new();
    let mut seen = HashSet::new();
    queue.push_back(start);

    while let Some(current) = queue.pop_front() {
        let Type::Class(ClassType { def, args }) = current.clone() else {
            continue;
        };
        if !seen.insert((def, args.clone())) {
            continue;
        }

        let Some(class_def) = env.class(def) else {
            continue;
        };
        let subst = class_def
            .type_params
            .iter()
            .copied()
            .zip(args.into_iter())
            .collect::<HashMap<_, _>>();

        for method in &class_def.methods {
            if method.name == name {
                out.push(CandidateMethod {
                    owner: def,
                    method: method.clone(),
                    class_subst: subst.clone(),
                });
            }
        }

        if let Some(sc) = &class_def.super_class {
            queue.push_back(substitute(sc, &subst));
        }
        for iface in &class_def.interfaces {
            queue.push_back(substitute(iface, &subst));
        }
    }

    out
}

fn check_applicability(
    env: &dyn TypeEnv,
    cand: &CandidateMethod,
    call: &MethodCall<'_>,
    phase: MethodPhase,
) -> Option<ResolvedMethod> {
    let method = &cand.method;
    let arity = call.args.len();

    // Arity precheck.
    match (method.is_varargs, phase) {
        (false, _) if arity != method.params.len() => return None,
        (true, MethodPhase::Strict | MethodPhase::Loose) if arity != method.params.len() => return None,
        (true, MethodPhase::Varargs) if arity + 1 < method.params.len() => return None,
        _ => {}
    }

    // Substitute class type parameters into the method signature.
    let base_params = method
        .params
        .iter()
        .map(|t| substitute(t, &cand.class_subst))
        .collect::<Vec<_>>();
    let base_return_type = substitute(&method.return_type, &cand.class_subst);

    // Try a fixed-arity invocation first (including varargs methods invoked with an array).
    if let Some(res) = try_method_invocation(env, cand.owner, method, &base_params, &base_return_type, call, phase, false)
    {
        return Some(res);
    }

    // Varargs phase can also use variable-arity invocation.
    if method.is_varargs && phase == MethodPhase::Varargs {
        return try_method_invocation(env, cand.owner, method, &base_params, &base_return_type, call, phase, true);
    }

    None
}

fn try_method_invocation(
    env: &dyn TypeEnv,
    owner: ClassId,
    method: &MethodDef,
    base_params: &[Type],
    base_return_type: &Type,
    call: &MethodCall<'_>,
    phase: MethodPhase,
    force_varargs: bool,
) -> Option<ResolvedMethod> {
    let arity = call.args.len();

    let (pattern_params, used_varargs) = if method.is_varargs && (phase == MethodPhase::Varargs) {
        if force_varargs {
            (expand_varargs_pattern(base_params, arity)?, true)
        } else {
            // Fixed-arity invocation (last param is the array type).
            if arity != base_params.len() {
                return None;
            }
            (base_params.to_vec(), false)
        }
    } else if method.is_varargs {
        // Strict/loose: only fixed-arity invocation allowed.
        if arity != base_params.len() {
            return None;
        }
        (base_params.to_vec(), false)
    } else {
        (base_params.to_vec(), false)
    };

    // Infer (or apply explicit) method type arguments using the effective parameter pattern.
    let inferred_type_args = if !method.type_params.is_empty() {
        if !call.explicit_type_args.is_empty() {
            call.explicit_type_args.clone()
        } else {
            infer_type_arguments_from_call(env, method, &pattern_params, base_return_type, call)
        }
    } else {
        Vec::new()
    };

    let method_subst: HashMap<TypeVarId, Type> = method
        .type_params
        .iter()
        .copied()
        .zip(inferred_type_args.iter().cloned())
        .collect();

    let effective_params: Vec<Type> = pattern_params
        .iter()
        .map(|t| substitute(t, &method_subst))
        .collect();
    if effective_params.len() != arity {
        return None;
    }
    let return_type = substitute(base_return_type, &method_subst);

    let mut warnings = Vec::new();
    for (arg, param) in call.args.iter().zip(&effective_params) {
        let conv = match phase {
            MethodPhase::Strict => strict_method_invocation_conversion(env, arg, param)?,
            MethodPhase::Loose | MethodPhase::Varargs => method_invocation_conversion(env, arg, param)?,
        };
        warnings.extend(conv.warnings);
    }

    Some(ResolvedMethod {
        owner,
        name: method.name.clone(),
        params: effective_params,
        return_type,
        is_varargs: method.is_varargs,
        inferred_type_args,
        warnings,
        used_varargs,
    })
}

fn expand_varargs_pattern(params: &[Type], arity: usize) -> Option<Vec<Type>> {
    if params.is_empty() {
        return Some(Vec::new());
    }
    let fixed = params.len().saturating_sub(1);
    if arity < fixed {
        return None;
    }

    let mut out = Vec::with_capacity(arity.max(params.len()));
    out.extend(params[..fixed].iter().cloned());
    let vararg_ty = params[fixed].clone();
    let elem_ty = match vararg_ty {
        Type::Array(elem) => *elem,
        other => other,
    };
    let extra = arity.saturating_sub(fixed);
    for _ in 0..extra {
        out.push(elem_ty.clone());
    }
    Some(out)
}

#[derive(Default, Clone)]
struct InferenceBounds {
    lower: Vec<Type>,
    upper: Vec<Type>,
}

fn infer_type_arguments_from_call(
    env: &dyn TypeEnv,
    method: &MethodDef,
    params: &[Type],
    return_type: &Type,
    call: &MethodCall<'_>,
) -> Vec<Type> {
    let object = Type::class(env.well_known().object, vec![]);
    let mut bounds: HashMap<TypeVarId, InferenceBounds> = method
        .type_params
        .iter()
        .copied()
        .map(|tv| {
            let mut b = InferenceBounds::default();
            b.upper.extend(
                env.type_param(tv)
                    .and_then(|tp| {
                        if tp.upper_bounds.is_empty() {
                            None
                        } else {
                            Some(tp.upper_bounds.clone())
                        }
                    })
                    .unwrap_or_else(|| vec![object.clone()]),
            );
            (tv, b)
        })
        .collect();

    // Constraints from arguments.
    for (arg, param) in call.args.iter().zip(params) {
        collect_arg_constraints(env, arg, param, &mut bounds);
    }

    // Constraints from expected return type.
    if let Some(expected) = &call.expected_return {
        collect_return_constraints(env, return_type, expected, &mut bounds);
    }

    // Solve bounds: prefer LUB of lowers, else GLB of uppers.
    method
        .type_params
        .iter()
        .map(|tv| {
            let b = bounds.get(tv).cloned().unwrap_or_default();
            let upper_glb = glb_all(env, &b.upper, &object);
            let candidate = if b.lower.is_empty() {
                upper_glb.clone()
            } else {
                lub_all(env, &b.lower, &object)
            };

            if is_subtype(env, &candidate, &upper_glb) {
                candidate
            } else {
                upper_glb
            }
        })
        .collect()
}

fn glb_all(env: &dyn TypeEnv, tys: &[Type], object: &Type) -> Type {
    let mut it = tys.iter();
    let Some(first) = it.next() else {
        return object.clone();
    };
    let mut acc = first.clone();
    for t in it {
        acc = glb(env, &acc, t);
    }
    acc
}

fn lub_all(env: &dyn TypeEnv, tys: &[Type], object: &Type) -> Type {
    let mut it = tys.iter();
    let Some(first) = it.next() else {
        return object.clone();
    };
    let mut acc = first.clone();
    for t in it {
        acc = lub2(env, &acc, t, object);
    }
    acc
}

fn lub2(env: &dyn TypeEnv, a: &Type, b: &Type, object: &Type) -> Type {
    if is_subtype(env, a, b) {
        return b.clone();
    }
    if is_subtype(env, b, a) {
        return a.clone();
    }
    object.clone()
}

fn push_lower_bound(bounds: &mut HashMap<TypeVarId, InferenceBounds>, tv: TypeVarId, ty: Type) {
    if is_placeholder_type_for_inference(&ty) {
        return;
    }
    if let Some(b) = bounds.get_mut(&tv) {
        b.lower.push(ty);
    }
}

fn push_upper_bound(bounds: &mut HashMap<TypeVarId, InferenceBounds>, tv: TypeVarId, ty: Type) {
    if is_placeholder_type_for_inference(&ty) {
        return;
    }
    if let Some(b) = bounds.get_mut(&tv) {
        b.upper.push(ty);
    }
}

fn collect_arg_constraints(
    env: &dyn TypeEnv,
    arg: &Type,
    param: &Type,
    bounds: &mut HashMap<TypeVarId, InferenceBounds>,
) {
    match param {
        Type::TypeVar(tv) => {
            push_lower_bound(bounds, *tv, arg.clone());
        }
        Type::Array(p_elem) => {
            if let Type::Array(a_elem) = arg {
                collect_arg_constraints(env, a_elem, p_elem, bounds);
            }
        }
        Type::Class(ClassType { def: p_def, args: p_args }) => {
            if let Type::Class(ClassType { def: a_def, args: a_args }) = arg {
                if p_def == a_def && p_args.len() == a_args.len() {
                    for (a, p) in a_args.iter().zip(p_args) {
                        collect_type_arg_constraints(env, a, p, bounds);
                    }
                }
            }
        }
        Type::Intersection(parts) => {
            for p in parts {
                collect_arg_constraints(env, arg, p, bounds);
            }
        }
        _ => {}
    }
}

fn collect_type_arg_constraints(
    env: &dyn TypeEnv,
    actual: &Type,
    formal: &Type,
    bounds: &mut HashMap<TypeVarId, InferenceBounds>,
) {
    match formal {
        Type::Wildcard(WildcardBound::Unbounded) => {}
        Type::Wildcard(WildcardBound::Extends(upper)) => {
            collect_arg_constraints(env, actual, upper, bounds);
        }
        Type::Wildcard(WildcardBound::Super(lower)) => {
            collect_reverse_constraints(env, lower, actual, bounds);
        }
        _ => collect_equality_constraints(env, actual, formal, bounds),
    }
}

fn collect_reverse_constraints(
    env: &dyn TypeEnv,
    lower: &Type,
    actual: &Type,
    bounds: &mut HashMap<TypeVarId, InferenceBounds>,
) {
    // lower <: actual
    match lower {
        Type::TypeVar(tv) => push_upper_bound(bounds, *tv, actual.clone()),
        Type::Class(ClassType { def: l_def, args: l_args }) => {
            if let Type::Class(ClassType { def: a_def, args: a_args }) = actual {
                if l_def == a_def && l_args.len() == a_args.len() {
                    for (l, a) in l_args.iter().zip(a_args) {
                        collect_reverse_constraints(env, l, a, bounds);
                    }
                }
            }
        }
        _ => {}
    }
}

fn collect_equality_constraints(
    env: &dyn TypeEnv,
    actual: &Type,
    formal: &Type,
    bounds: &mut HashMap<TypeVarId, InferenceBounds>,
) {
    match formal {
        Type::TypeVar(tv) => {
            push_lower_bound(bounds, *tv, actual.clone());
            push_upper_bound(bounds, *tv, actual.clone());
        }
        Type::Array(f_elem) => {
            if let Type::Array(a_elem) = actual {
                collect_equality_constraints(env, a_elem, f_elem, bounds);
            }
        }
        Type::Class(ClassType { def: f_def, args: f_args }) => {
            if let Type::Class(ClassType { def: a_def, args: a_args }) = actual {
                if f_def == a_def && f_args.len() == a_args.len() {
                    for (a, f) in a_args.iter().zip(f_args) {
                        collect_equality_constraints(env, a, f, bounds);
                    }
                }
            }
        }
        _ => {}
    }
}

fn collect_return_constraints(
    env: &dyn TypeEnv,
    ret: &Type,
    expected: &Type,
    bounds: &mut HashMap<TypeVarId, InferenceBounds>,
) {
    // ret <: expected
    match ret {
        Type::TypeVar(tv) => push_upper_bound(bounds, *tv, expected.clone()),
        Type::Class(ClassType { def: r_def, args: r_args }) => {
            if let Type::Class(ClassType { def: e_def, args: e_args }) = expected {
                if r_def == e_def && r_args.len() == e_args.len() {
                    for (r, e) in r_args.iter().zip(e_args) {
                        collect_equality_constraints(env, e, r, bounds);
                    }
                }
            }
        }
        Type::Array(r_elem) => {
            if let Type::Array(e_elem) = expected {
                collect_return_constraints(env, r_elem, e_elem, bounds);
            }
        }
        Type::Intersection(parts) => {
            for p in parts {
                collect_return_constraints(env, p, expected, bounds);
            }
        }
        _ => {}
    }
}

fn collect_type_var_constraints(mapping: &mut HashMap<TypeVarId, Type>, pattern: &Type, actual: &Type) {
    match pattern {
        Type::TypeVar(id) => insert_type_var_constraint(mapping, *id, actual),
        Type::Array(p_elem) => {
            if let Type::Array(a_elem) = actual {
                collect_type_var_constraints(mapping, p_elem, a_elem);
            }
        }
        Type::Class(ClassType { def: p_def, args: p_args }) => {
            if let Type::Class(ClassType { def: a_def, args: a_args }) = actual {
                if p_def == a_def && p_args.len() == a_args.len() {
                    for (p, a) in p_args.iter().zip(a_args) {
                        collect_type_var_constraints(mapping, p, a);
                    }
                }
            }
        }
        Type::Wildcard(WildcardBound::Extends(p)) | Type::Wildcard(WildcardBound::Super(p)) => {
            collect_type_var_constraints(mapping, p, actual);
        }
        Type::Intersection(types) => {
            for t in types {
                collect_type_var_constraints(mapping, t, actual);
            }
        }
        _ => {}
    }
}

fn insert_type_var_constraint(mapping: &mut HashMap<TypeVarId, Type>, id: TypeVarId, actual: &Type) {
    use std::collections::hash_map::Entry;

    match mapping.entry(id) {
        Entry::Vacant(v) => {
            v.insert(actual.clone());
        }
        Entry::Occupied(mut o) => {
            let current = o.get();
            if is_placeholder_type_for_inference(current) && !is_placeholder_type_for_inference(actual) {
                o.insert(actual.clone());
            }
        }
    }
}

fn is_placeholder_type_for_inference(ty: &Type) -> bool {
    matches!(ty, Type::Unknown | Type::Error | Type::Null)
}

fn most_specific<'a>(
    env: &dyn TypeEnv,
    methods: &'a [ResolvedMethod],
    arity: usize,
) -> Option<&'a ResolvedMethod> {
    if methods.is_empty() {
        return None;
    }

    let mut maximals = Vec::new();
    'outer: for m in methods {
        for other in methods {
            if std::ptr::eq(m, other) {
                continue;
            }
            if !is_more_specific(env, m, other, arity) {
                continue 'outer;
            }
        }
        maximals.push(m);
    }

    if maximals.len() == 1 {
        return Some(maximals[0]);
    }

    if maximals.is_empty() {
        return None;
    }

    // Tie-breakers (best-effort):
    // 1. Prefer fixed-arity invocations over varargs expansion.
    // 2. Prefer non-generic methods over generic ones when parameter types tie.
    // 3. Prefer fewer unchecked/raw warnings.
    let mut scored: Vec<(&ResolvedMethod, (u8, u8, usize))> = maximals
        .into_iter()
        .map(|m| {
            (
                m,
                (
                    u8::from(m.used_varargs),
                    u8::from(!m.inferred_type_args.is_empty()),
                    m.warnings.len(),
                ),
            )
        })
        .collect();

    scored.sort_by(|a, b| a.1.cmp(&b.1));
    let (best, best_score) = scored.first().copied().unwrap();
    if scored.iter().skip(1).all(|(_, score)| score != &best_score) {
        Some(best)
    } else {
        None
    }
}

fn is_more_specific(env: &dyn TypeEnv, a: &ResolvedMethod, b: &ResolvedMethod, arity: usize) -> bool {
    if a.used_varargs != b.used_varargs {
        return !a.used_varargs && b.used_varargs;
    }

    if a.params.len() != arity || b.params.len() != arity {
        return false;
    }

    a.params
        .iter()
        .zip(&b.params)
        .all(|(a_ty, b_ty)| is_subtype(env, a_ty, b_ty))
}

// === Inference helpers =======================================================

pub fn infer_var_type(initializer: Option<Type>) -> Type {
    initializer.unwrap_or(Type::Error)
}

/// Infer type arguments for a generic method given a call site.
///
/// This is a small, constraint-based solver (far from full JLS 18), but it's
/// sufficient for common IDE use-cases.
///
/// The `owner` is the declaring class/interface of `method`.
pub fn infer_type_arguments(
    env: &dyn TypeEnv,
    call: &MethodCall<'_>,
    owner: ClassId,
    method: &MethodDef,
) -> Vec<Type> {
    if method.type_params.is_empty() {
        return Vec::new();
    }

    if !call.explicit_type_args.is_empty() {
        return call.explicit_type_args.clone();
    }

    let mut receiver = call.receiver.clone();
    if let Type::Named(name) = &receiver {
        if let Some(id) = env.lookup_class(name) {
            receiver = Type::class(id, vec![]);
        }
    }

    let class_subst = class_substitution_for_owner(env, &receiver, owner);
    let params = method
        .params
        .iter()
        .map(|t| substitute(t, &class_subst))
        .collect::<Vec<_>>();
    let return_type = substitute(&method.return_type, &class_subst);

    infer_type_arguments_from_call(env, method, &params, &return_type, call)
}

pub fn infer_diamond_type_args(env: &dyn TypeEnv, class: ClassId, target: Option<&Type>) -> Vec<Type> {
    let Some(class_def) = env.class(class) else {
        return Vec::new();
    };

    if class_def.type_params.is_empty() {
        return Vec::new();
    }

    if let Some(Type::Class(ClassType { def, args })) = target {
        if *def == class && args.len() == class_def.type_params.len() {
            return args.clone();
        }
    }

    let object = Type::class(env.well_known().object, vec![]);

    // Best-effort: infer type parameters from a supertype target, e.g.
    // `List<String> xs = new ArrayList<>()` => `ArrayList<String>`.
    if let Some(target_ty) = target {
        let target_class = match target_ty {
            Type::Class(ct) => Some(ct.clone()),
            Type::Named(name) => env.lookup_class(name).map(|id| ClassType { def: id, args: vec![] }),
            _ => None,
        };

        if let Some(target_class) = target_class {
            if !target_class.args.is_empty() {
                if let Some(mapping) =
                    infer_class_type_arguments_from_target(env, class, target_class.def, &target_class.args)
                {
                    return class_def
                        .type_params
                        .iter()
                        .map(|id| mapping.get(id).cloned().unwrap_or_else(|| object.clone()))
                        .collect();
                }
            }
        }
    }

    // Fall back to Object for each type parameter.
    vec![object; class_def.type_params.len()]
}

pub fn infer_lambda_param_types(env: &dyn TypeEnv, target: &Type) -> Option<Vec<Type>> {
    let Type::Class(ClassType { def, args }) = target else {
        return None;
    };
    let class_def = env.class(*def)?;

    // Build substitution for the interface type parameters.
    let subst = class_def
        .type_params
        .iter()
        .copied()
        .zip(args.iter().cloned())
        .collect::<HashMap<_, _>>();

    // Find a single abstract method.
    let abstract_methods: Vec<&MethodDef> = class_def
        .methods
        .iter()
        .filter(|m| !m.is_static && m.is_abstract)
        .collect();

    if abstract_methods.len() != 1 {
        return None;
    }

    let sam = abstract_methods[0];
    let params = sam
        .params
        .iter()
        .map(|t| substitute(t, &subst))
        .collect();
    Some(params)
}

fn class_substitution_for_owner(
    env: &dyn TypeEnv,
    receiver: &Type,
    owner: ClassId,
) -> HashMap<TypeVarId, Type> {
    let mut subst = HashMap::new();

    let Type::Class(ClassType { def, args }) = receiver else {
        return subst;
    };

    let owner_instantiation = instantiate_as(env, *def, args.clone(), owner);
    let Some(owner_instantiation) = owner_instantiation else {
        return subst;
    };

    let Some(owner_def) = env.class(owner) else {
        return subst;
    };

    if owner_def.type_params.len() != owner_instantiation.len() {
        return subst;
    }

    subst.extend(
        owner_def
            .type_params
            .iter()
            .copied()
            .zip(owner_instantiation.into_iter()),
    );

    subst
}

fn instantiate_as(
    env: &dyn TypeEnv,
    start_def: ClassId,
    start_args: Vec<Type>,
    target_def: ClassId,
) -> Option<Vec<Type>> {
    let mut queue = VecDeque::new();
    let mut seen = HashSet::new();
    queue.push_back(Type::class(start_def, start_args));

    while let Some(current) = queue.pop_front() {
        let Type::Class(ClassType { def, args }) = current.clone() else {
            continue;
        };
        if !seen.insert((def, args.clone())) {
            continue;
        }

        if def == target_def {
            return Some(args);
        }

        let Some(class_def) = env.class(def) else {
            continue;
        };
        let subst = class_def
            .type_params
            .iter()
            .copied()
            .zip(args.into_iter())
            .collect::<HashMap<_, _>>();

        if let Some(sc) = &class_def.super_class {
            queue.push_back(substitute(sc, &subst));
        }
        for iface in &class_def.interfaces {
            queue.push_back(substitute(iface, &subst));
        }
    }

    None
}

fn infer_class_type_arguments_from_target(
    env: &dyn TypeEnv,
    class: ClassId,
    target_def: ClassId,
    target_args: &[Type],
) -> Option<HashMap<TypeVarId, Type>> {
    let class_def = env.class(class)?;
    if class_def.type_params.is_empty() {
        return None;
    }

    // Start with a symbolic instantiation: `C<T1, T2, ...>`.
    let start_args = class_def
        .type_params
        .iter()
        .copied()
        .map(Type::TypeVar)
        .collect::<Vec<_>>();
    let mut queue = VecDeque::new();
    let mut seen = HashSet::new();
    queue.push_back(Type::class(class, start_args));

    while let Some(current) = queue.pop_front() {
        let Type::Class(ClassType { def, args }) = current.clone() else {
            continue;
        };
        if !seen.insert((def, args.clone())) {
            continue;
        }

        if def == target_def {
            if args.len() != target_args.len() {
                return None;
            }
            let mut mapping = HashMap::new();
            for (pattern, actual) in args.iter().zip(target_args) {
                collect_type_var_constraints(&mut mapping, pattern, actual);
            }
            return Some(mapping);
        }

        let Some(current_def) = env.class(def) else {
            continue;
        };

        let subst = current_def
            .type_params
            .iter()
            .copied()
            .zip(args.into_iter())
            .collect::<HashMap<_, _>>();

        if let Some(sc) = &current_def.super_class {
            queue.push_back(substitute(sc, &subst));
        }
        for iface in &current_def.interfaces {
            queue.push_back(substitute(iface, &subst));
        }
    }

    None
}

// === Minimal expression typing ==============================================

/// A tiny expression model used for unit tests and as an example integration.
#[derive(Debug, Clone)]
pub enum Expr {
    Null,
    Int(i32),
    String(String),
    MethodCall {
        receiver: Box<Expr>,
        name: String,
        args: Vec<Expr>,
        expected_return: Option<Type>,
    },
}

pub fn type_of(env: &mut TypeStore, expr: &Expr) -> Type {
    match expr {
        Expr::Null => Type::Null,
        Expr::Int(_) => Type::Primitive(PrimitiveType::Int),
        Expr::String(_) => Type::class(env.well_known().string, vec![]),
        Expr::MethodCall {
            receiver,
            name,
            args,
            expected_return,
        } => {
            let recv_ty = type_of(env, receiver);
            let arg_tys = args.iter().map(|a| type_of(env, a)).collect::<Vec<_>>();
            let call = MethodCall {
                receiver: recv_ty,
                name,
                args: arg_tys,
                expected_return: expected_return.clone(),
                explicit_type_args: vec![],
            };
            match resolve_method_call(env, &call) {
                MethodResolution::Found(m) => m.return_type,
                _ => Type::Error,
            }
        }
    }
}

// === Tests ==================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> TypeStore {
        TypeStore::with_minimal_jdk()
    }

    #[test]
    fn primitive_widening_assignable() {
        let env = store();
        assert!(is_assignable(
            &env,
            &Type::Primitive(PrimitiveType::Int),
            &Type::Primitive(PrimitiveType::Long)
        ));
        assert!(!is_assignable(
            &env,
            &Type::Primitive(PrimitiveType::Long),
            &Type::Primitive(PrimitiveType::Int)
        ));
    }

    #[test]
    fn null_assignable_to_reference() {
        let env = store();
        let obj = Type::class(env.well_known().object, vec![]);
        assert!(is_assignable(&env, &Type::Null, &obj));
    }

    #[test]
    fn type_store_resolves_java_lang_simple_names() {
        let env = store();
        assert_eq!(env.class_id("String"), Some(env.well_known().string));
        assert_eq!(env.class_id("Object"), Some(env.well_known().object));
    }

    #[test]
    fn simple_class_inheritance() {
        let mut env = store();
        let object = env.well_known().object;

        let animal = env.add_class(ClassDef {
            name: "Animal".to_string(),
            kind: ClassKind::Class,
            type_params: vec![],
            super_class: Some(Type::class(object, vec![])),
            interfaces: vec![],
            methods: vec![],
        });
        let dog = env.add_class(ClassDef {
            name: "Dog".to_string(),
            kind: ClassKind::Class,
            type_params: vec![],
            super_class: Some(Type::class(animal, vec![])),
            interfaces: vec![],
            methods: vec![],
        });

        assert!(is_subtype(
            &env,
            &Type::class(dog, vec![]),
            &Type::class(animal, vec![])
        ));
        assert!(is_subtype(
            &env,
            &Type::class(dog, vec![]),
            &Type::class(object, vec![])
        ));
    }

    #[test]
    fn overload_resolution_prefers_more_specific() {
        let mut env = store();
        let object = env.well_known().object;
        let string = env.well_known().string;

        let foo = env.add_class(ClassDef {
            name: "Foo".to_string(),
            kind: ClassKind::Class,
            type_params: vec![],
            super_class: Some(Type::class(object, vec![])),
            interfaces: vec![],
            methods: vec![
                MethodDef {
                    name: "m".to_string(),
                    type_params: vec![],
                    params: vec![Type::class(object, vec![])],
                    return_type: Type::Void,
                    is_static: false,
                    is_varargs: false,
                    is_abstract: false,
                },
                MethodDef {
                    name: "m".to_string(),
                    type_params: vec![],
                    params: vec![Type::class(string, vec![])],
                    return_type: Type::Void,
                    is_static: false,
                    is_varargs: false,
                    is_abstract: false,
                },
            ],
        });

        let call = MethodCall {
            receiver: Type::class(foo, vec![]),
            name: "m",
            args: vec![Type::class(string, vec![])],
            expected_return: None,
            explicit_type_args: vec![],
        };

        let MethodResolution::Found(found) = resolve_method_call(&mut env, &call) else {
            panic!("expected method to be resolved");
        };
        assert_eq!(found.params, vec![Type::class(string, vec![])]);
    }

    #[test]
    fn var_inference_from_initializer() {
        let env = store();
        let ty = infer_var_type(Some(Type::class(env.well_known().string, vec![])));
        assert_eq!(ty, Type::class(env.well_known().string, vec![]));
    }

    #[test]
    fn generic_inheritance_arraylist_to_list() {
        let env = store();
        let string = Type::class(env.well_known().string, vec![]);
        let array_list = env.class_id("java.util.ArrayList").unwrap();
        let list = env.class_id("java.util.List").unwrap();

        let al_string = Type::class(array_list, vec![string.clone()]);
        let list_string = Type::class(list, vec![string.clone()]);
        let list_object = Type::class(list, vec![Type::class(env.well_known().object, vec![])]);

        assert!(is_subtype(&env, &al_string, &list_string));
        assert!(!is_subtype(&env, &list_string, &list_object));
    }

    #[test]
    fn diamond_inference_uses_target_supertype() {
        let env = store();
        let array_list = env.class_id("java.util.ArrayList").unwrap();
        let list = env.class_id("java.util.List").unwrap();

        let string = Type::class(env.well_known().string, vec![]);
        let target = Type::class(list, vec![string.clone()]);

        let inferred = infer_diamond_type_args(&env, array_list, Some(&target));
        assert_eq!(inferred, vec![string]);
    }

    #[test]
    fn infer_type_arguments_api_basic_generic_method() {
        let mut env = store();
        let object = env.well_known().object;
        let string = Type::class(env.well_known().string, vec![]);

        let t = env.add_type_param("T", vec![Type::class(object, vec![])]);
        let util = env.add_class(ClassDef {
            name: "Util".to_string(),
            kind: ClassKind::Class,
            type_params: vec![],
            super_class: Some(Type::class(object, vec![])),
            interfaces: vec![],
            methods: vec![MethodDef {
                name: "id".to_string(),
                type_params: vec![t],
                params: vec![Type::TypeVar(t)],
                return_type: Type::TypeVar(t),
                is_static: true,
                is_varargs: false,
                is_abstract: false,
            }],
        });

        let call = MethodCall {
            receiver: Type::class(util, vec![]),
            name: "id",
            args: vec![string.clone()],
            expected_return: None,
            explicit_type_args: vec![],
        };
        let method = &env.class(util).unwrap().methods[0];
        let inferred = infer_type_arguments(&env, &call, util, method);
        assert_eq!(inferred, vec![string]);
    }

    #[test]
    fn infer_type_arguments_prefers_expected_return_over_unknown_arg() {
        let mut env = store();
        let object = env.well_known().object;
        let string = Type::class(env.well_known().string, vec![]);

        let t = env.add_type_param("T", vec![Type::class(object, vec![])]);
        let util = env.add_class(ClassDef {
            name: "Util2".to_string(),
            kind: ClassKind::Class,
            type_params: vec![],
            super_class: Some(Type::class(object, vec![])),
            interfaces: vec![],
            methods: vec![MethodDef {
                name: "id".to_string(),
                type_params: vec![t],
                params: vec![Type::TypeVar(t)],
                return_type: Type::TypeVar(t),
                is_static: true,
                is_varargs: false,
                is_abstract: false,
            }],
        });

        let call = MethodCall {
            receiver: Type::class(util, vec![]),
            name: "id",
            args: vec![Type::Unknown],
            expected_return: Some(string.clone()),
            explicit_type_args: vec![],
        };
        let method = &env.class(util).unwrap().methods[0];
        let inferred = infer_type_arguments(&env, &call, util, method);
        assert_eq!(inferred, vec![string]);
    }

    #[test]
    fn lambda_param_inference_from_function_target() {
        let env = store();
        let function = env.class_id("java.util.function.Function").unwrap();
        let target = Type::class(
            function,
            vec![
                Type::class(env.well_known().string, vec![]),
                Type::class(env.well_known().integer, vec![]),
            ],
        );
        let params = infer_lambda_param_types(&env, &target).expect("should infer lambda params");
        assert_eq!(params, vec![Type::class(env.well_known().string, vec![])]);
    }
}

// -----------------------------------------------------------------------------
// Lightweight type spelling helpers (used by early refactorings)
// -----------------------------------------------------------------------------

/// A best-effort Java type spelling.
///
/// Nova's long-term type system will represent types structurally. For early
/// refactorings we primarily need a stable type *spelling* and heuristics for
/// deciding whether to insert imports for fully-qualified types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeRef {
    text: String,
}

impl TypeRef {
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// Returns a fully qualified base type (without generics/arrays) if one is
    /// present. Example: `com.example.Foo<Bar>[]` → `com.example.Foo`.
    pub fn fully_qualified_base(&self) -> Option<&str> {
        let base = self.base_type();
        base.contains('.').then_some(base)
    }

    /// Returns the type text but with the base type shortened to the simple
    /// name (dropping package qualifiers).
    pub fn with_simple_base(&self) -> String {
        let base = self.base_type();
        let Some((_, simple)) = base.rsplit_once('.') else {
            return self.text.clone();
        };
        self.text.replacen(base, simple, 1)
    }

    /// Whether this type should be imported when used in a compilation unit.
    ///
    /// Best-effort heuristics:
    /// - fully-qualified names outside `java.lang` get imported
    /// - primitive types and simple names do not
    pub fn needs_import(&self) -> bool {
        let Some(fq) = self.fully_qualified_base() else {
            return false;
        };
        !fq.starts_with("java.lang.")
    }

    fn base_type(&self) -> &str {
        let mut s = self.text.trim();
        if let Some(idx) = s.find('<') {
            s = &s[..idx];
        }
        while let Some(stripped) = s.strip_suffix("[]") {
            s = stripped;
        }
        s
    }
}

// -----------------------------------------------------------------------------
// Refactoring IDs
// -----------------------------------------------------------------------------

/// Identifies a method symbol within an `nova-index::Index` snapshot.
///
/// This is *not* stable across edits; it is intended to be resolved against a
/// single index snapshot before applying a refactoring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct MethodId(pub u32);

impl MethodId {
    pub fn new(raw: u32) -> Self {
        Self(raw)
    }
}
