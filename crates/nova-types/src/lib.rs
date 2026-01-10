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
            methods: vec![MethodDef {
                name: "get".to_string(),
                type_params: vec![],
                params: vec![Type::Primitive(PrimitiveType::Int)],
                return_type: Type::TypeVar(list_e),
                is_static: false,
                is_varargs: false,
                is_abstract: true,
            }],
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
        });
        id
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
        self.class_by_name.get(name).copied()
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
            // Type variables are reference types; treat `T` as `? extends upperBound`.
            env.type_param(*id)
                .map(|tp| {
                    if tp.upper_bounds.is_empty() {
                        other.is_reference()
                    } else {
                        tp.upper_bounds.iter().all(|b| is_subtype(env, other, b))
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
            return type_args_compatible(env, &args, &super_args);
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

fn type_args_compatible(env: &dyn TypeEnv, sub: &[Type], super_: &[Type]) -> bool {
    if sub.len() != super_.len() {
        // Raw vs parameterized mismatch; treat as incompatible for now.
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
    // Assignment conversion is broader than subtyping, but for our current needs we
    // mostly care about:
    // * primitive widening
    // * reference widening (subtyping)
    // * null to reference
    is_subtype(env, from, to)
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
}

#[derive(Debug, Clone)]
pub enum MethodResolution {
    Found(ResolvedMethod),
    NotFound,
    Ambiguous(Vec<ResolvedMethod>),
}

pub fn resolve_method_call(env: &dyn TypeEnv, call: &MethodCall<'_>) -> MethodResolution {
    let mut receiver = call.receiver.clone();
    if let Type::Named(name) = &receiver {
        if let Some(id) = env.lookup_class(name) {
            receiver = Type::class(id, vec![]);
        }
    }

    let mut candidates = collect_method_candidates(env, &receiver, call.name);

    if candidates.is_empty() {
        return MethodResolution::NotFound;
    }

    // Phase ordering: prefer fixed-arity over varargs (simplified JLS 15.12.2).
    let mut fixed_applicable = Vec::new();
    let mut varargs_applicable = Vec::new();

    for cand in candidates.drain(..) {
        if let Some(app) = check_applicability(env, &cand, call) {
            if app.is_varargs {
                varargs_applicable.push(app);
            } else {
                fixed_applicable.push(app);
            }
        }
    }

    let applicable = if !fixed_applicable.is_empty() {
        fixed_applicable
    } else {
        varargs_applicable
    };

    if applicable.is_empty() {
        return MethodResolution::NotFound;
    }

    match most_specific(env, &applicable, call.args.len()) {
        Some(best) => MethodResolution::Found(best.clone()),
        None => MethodResolution::Ambiguous(applicable),
    }
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
) -> Option<ResolvedMethod> {
    let method = &cand.method;

    // Arity check.
    if method.is_varargs {
        if call.args.len() < method.params.len().saturating_sub(1) {
            return None;
        }
    } else if call.args.len() != method.params.len() {
        return None;
    }

    // Substitute class type parameters into the method signature.
    let mut params = method
        .params
        .iter()
        .map(|t| substitute(t, &cand.class_subst))
        .collect::<Vec<_>>();
    let mut return_type = substitute(&method.return_type, &cand.class_subst);

    // Infer (or apply explicit) method type arguments.
    let inferred_type_args = if !method.type_params.is_empty() {
        if !call.explicit_type_args.is_empty() {
            call.explicit_type_args.clone()
        } else {
            infer_type_arguments_from_call(env, method, &params, &return_type, call)
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

    params = params.iter().map(|t| substitute(t, &method_subst)).collect();
    return_type = substitute(&return_type, &method_subst);

    let effective_params = if method.is_varargs {
        // Expand varargs to match the call site.
        MethodDef {
            name: method.name.clone(),
            type_params: vec![],
            params: params.clone(),
            return_type: return_type.clone(),
            is_static: method.is_static,
            is_varargs: true,
            is_abstract: method.is_abstract,
        }
        .param_types_for_arity(call.args.len())
    } else {
        params.clone()
    };

    if effective_params.len() != call.args.len() {
        return None;
    }

    for (arg, param) in call.args.iter().zip(&effective_params) {
        if !is_assignable(env, arg, param) {
            return None;
        }
    }

    Some(ResolvedMethod {
        owner: cand.owner,
        name: method.name.clone(),
        params: effective_params,
        return_type,
        is_varargs: method.is_varargs,
        inferred_type_args,
    })
}

fn infer_type_arguments_from_call(
    env: &dyn TypeEnv,
    method: &MethodDef,
    params: &[Type],
    return_type: &Type,
    call: &MethodCall<'_>,
) -> Vec<Type> {
    let mut mapping: HashMap<TypeVarId, Type> = HashMap::new();

    // Constraints from arguments.
    if method.is_varargs && !params.is_empty() {
        let fixed = params.len() - 1;
        for (arg, param) in call.args.iter().take(fixed).zip(&params[..fixed]) {
            collect_type_var_constraints(&mut mapping, param, arg);
        }

        let vararg_param = &params[fixed];
        let elem_ty = match vararg_param {
            Type::Array(elem) => elem.as_ref(),
            other => other,
        };
        for arg in call.args.iter().skip(fixed) {
            collect_type_var_constraints(&mut mapping, elem_ty, arg);
        }
    } else {
        for (arg, param) in call.args.iter().zip(params) {
            collect_type_var_constraints(&mut mapping, param, arg);
        }
    }

    // Constraints from expected return type.
    if let Some(expected) = &call.expected_return {
        collect_type_var_constraints(&mut mapping, return_type, expected);
    }

    // Solve: fill with discovered mapping or upper bounds (or Object).
    let object = Type::class(env.well_known().object, vec![]);
    method
        .type_params
        .iter()
        .map(|id| {
            mapping.get(id).cloned().unwrap_or_else(|| {
                env.type_param(*id)
                    .and_then(|tp| tp.upper_bounds.first().cloned())
                    .unwrap_or_else(|| object.clone())
            })
        })
        .collect()
}

fn collect_type_var_constraints(
    mapping: &mut HashMap<TypeVarId, Type>,
    pattern: &Type,
    actual: &Type,
) {
    match pattern {
        Type::TypeVar(id) => {
            mapping.entry(*id).or_insert_with(|| actual.clone());
        }
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
        Type::Wildcard(WildcardBound::Extends(p)) => {
            collect_type_var_constraints(mapping, p, actual);
        }
        Type::Wildcard(WildcardBound::Super(p)) => {
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

fn most_specific<'a>(
    env: &dyn TypeEnv,
    methods: &'a [ResolvedMethod],
    arity: usize,
) -> Option<&'a ResolvedMethod> {
    let mut best: Option<&ResolvedMethod> = None;

    'outer: for m in methods {
        for other in methods {
            if std::ptr::eq(m, other) {
                continue;
            }

            if !is_more_specific(env, m, other, arity) {
                continue 'outer;
            }
        }
        if best.is_some() {
            // Two methods are "most specific" => ambiguous.
            return None;
        }
        best = Some(m);
    }

    best
}

fn is_more_specific(env: &dyn TypeEnv, a: &ResolvedMethod, b: &ResolvedMethod, arity: usize) -> bool {
    let a_params = if a.is_varargs {
        if a.params.len() == arity {
            &a.params
        } else {
            return false;
        }
    } else {
        &a.params
    };
    let b_params = if b.is_varargs {
        if b.params.len() == arity {
            &b.params
        } else {
            return false;
        }
    } else {
        &b.params
    };

    if a_params.len() != b_params.len() {
        return false;
    }

    a_params
        .iter()
        .zip(b_params)
        .all(|(a_ty, b_ty)| is_subtype(env, a_ty, b_ty))
}

// === Inference helpers =======================================================

pub fn infer_var_type(initializer: Option<Type>) -> Type {
    initializer.unwrap_or(Type::Error)
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

    // Fall back to Object for each type parameter.
    let object = Type::class(env.well_known().object, vec![]);
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

pub fn type_of(env: &dyn TypeEnv, expr: &Expr) -> Type {
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

        let MethodResolution::Found(found) = resolve_method_call(&env, &call) else {
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
