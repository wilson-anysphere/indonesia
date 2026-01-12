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

use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub mod java;

pub use java::env::TyContext;
pub use java::helpers::{instantiate_as_supertype, sam_signature, SamSignature};
pub use java::overload::resolve_method_call;

pub use java::format::{
    format_method_signature, format_resolved_method, format_type, MethodSignatureDisplay,
    ResolvedMethodDisplay, TypeDisplay,
};

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
    pub code: Cow<'static, str>,
    pub message: String,
    pub span: Option<Span>,
}

impl Diagnostic {
    pub fn error(
        code: impl Into<Cow<'static, str>>,
        message: impl Into<String>,
        span: Option<Span>,
    ) -> Self {
        Self {
            severity: Severity::Error,
            code: code.into(),
            message: message.into(),
            span,
        }
    }

    pub fn warning(
        code: impl Into<Cow<'static, str>>,
        message: impl Into<String>,
        span: Option<Span>,
    ) -> Self {
        Self {
            severity: Severity::Warning,
            code: code.into(),
            message: message.into(),
            span,
        }
    }
}

#[cfg(test)]
mod diagnostic_tests {
    use super::*;

    #[test]
    fn static_codes_are_borrowed() {
        let diag = Diagnostic::error("SYNTAX", "msg", None);
        assert!(matches!(diag.code, Cow::Borrowed("SYNTAX")));
    }

    #[test]
    fn dynamic_codes_can_be_owned() {
        let diag = Diagnostic {
            severity: Severity::Error,
            code: Cow::Owned("my.plugin.code".to_string()),
            message: "msg".to_string(),
            span: None,
        };

        assert_eq!(diag.code.as_ref(), "my.plugin.code");

        // Ensure `Diagnostic` stays `Clone + Eq`.
        let cloned = diag.clone();
        assert_eq!(cloned, diag);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionItem {
    pub label: String,
    pub detail: Option<String>,
    pub replace_span: Option<Span>,
}

impl CompletionItem {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            detail: None,
            replace_span: None,
        }
    }
}

// -----------------------------------------------------------------------------
// Framework/type-checker stubs
// -----------------------------------------------------------------------------

pub use nova_ids::{ClassId, ProjectId};

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
///
/// To materialize these stubs into a [`TypeStore`], use the canonical loader in the
/// `nova-types-bridge` crate (`ExternalTypeLoader`).
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
                if m.is_empty() {
                    None
                } else {
                    Some(m)
                }
            })
            .unwrap_or_default()
    }

    fn supertypes(&self, binary_name: &str) -> Vec<String> {
        self.providers
            .iter()
            .find_map(|p| {
                let s = p.supertypes(binary_name);
                if s.is_empty() {
                    None
                } else {
                    Some(s)
                }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallKind {
    Static,
    Instance,
}

#[derive(Debug, Clone)]
pub struct TypeParamDef {
    pub name: String,
    pub upper_bounds: Vec<Type>,
    /// Capture conversion may introduce a lower bound (`? super T`).
    pub lower_bound: Option<Type>,
}

#[derive(Debug, Clone)]
pub struct FieldDef {
    pub name: String,
    pub ty: Type,
    pub is_static: bool,
    pub is_final: bool,
}

#[derive(Debug, Clone)]
pub struct ConstructorDef {
    pub params: Vec<Type>,
    pub is_varargs: bool,
    /// Best-effort accessibility bit (e.g. `private` constructors are marked
    /// inaccessible). Full accessibility rules depend on the call-site context
    /// and will be handled by higher semantic layers.
    pub is_accessible: bool,
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
    pub fields: Vec<FieldDef>,
    pub constructors: Vec<ConstructorDef>,
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

#[derive(Debug)]
pub struct TypeStore {
    classes: Vec<ClassDef>,
    class_by_name: HashMap<String, ClassId>,
    tombstones: HashMap<String, ClassId>,
    type_params: Vec<TypeParamDef>,
    well_known: Option<WellKnownTypes>,
}

impl Clone for TypeStore {
    fn clone(&self) -> Self {
        Self {
            classes: self.classes.clone(),
            class_by_name: self.class_by_name.clone(),
            tombstones: self.tombstones.clone(),
            type_params: self.type_params.clone(),
            well_known: self.well_known.clone(),
        }
    }
}

impl Default for TypeStore {
    fn default() -> Self {
        let mut store = Self {
            classes: Vec::new(),
            class_by_name: HashMap::new(),
            tombstones: HashMap::new(),
            type_params: Vec::new(),
            well_known: None,
        };

        // `nova-types` algorithms assume a baseline set of well-known JDK types
        // always exists. Initializing these here avoids a common footgun where
        // callers construct `TypeStore::default()` but forget to run a loader
        // bootstrap step before calling into subtyping/LUB/etc.
        let object = store.intern_class_id("java.lang.Object");
        let object_ty = Type::class(object, vec![]);

        let string = store.intern_class_id("java.lang.String");
        let string_ty = Type::class(string, vec![]);
        let integer = store.intern_class_id("java.lang.Integer");
        let cloneable = store.intern_class_id("java.lang.Cloneable");
        let serializable = store.intern_class_id("java.io.Serializable");

        store.define_class(
            object,
            ClassDef {
                name: "java.lang.Object".to_string(),
                kind: ClassKind::Class,
                type_params: vec![],
                super_class: None,
                interfaces: vec![],
                fields: vec![],
                constructors: vec![ConstructorDef {
                    params: vec![],
                    is_varargs: false,
                    is_accessible: true,
                }],
                methods: vec![
                    MethodDef {
                        name: "toString".to_string(),
                        type_params: vec![],
                        params: vec![],
                        return_type: string_ty.clone(),
                        is_static: false,
                        is_varargs: false,
                        is_abstract: false,
                    },
                    MethodDef {
                        name: "equals".to_string(),
                        type_params: vec![],
                        params: vec![object_ty.clone()],
                        return_type: Type::Primitive(PrimitiveType::Boolean),
                        is_static: false,
                        is_varargs: false,
                        is_abstract: false,
                    },
                    MethodDef {
                        name: "hashCode".to_string(),
                        type_params: vec![],
                        params: vec![],
                        return_type: Type::Primitive(PrimitiveType::Int),
                        is_static: false,
                        is_varargs: false,
                        is_abstract: false,
                    },
                ],
            },
        );
        store.define_class(
            string,
            ClassDef {
                name: "java.lang.String".to_string(),
                kind: ClassKind::Class,
                type_params: vec![],
                super_class: Some(object_ty.clone()),
                interfaces: vec![],
                fields: vec![],
                constructors: vec![],
                methods: vec![],
            },
        );
        store.define_class(
            integer,
            ClassDef {
                name: "java.lang.Integer".to_string(),
                kind: ClassKind::Class,
                type_params: vec![],
                super_class: Some(object_ty.clone()),
                interfaces: vec![],
                fields: vec![],
                constructors: vec![],
                methods: vec![],
            },
        );
        store.define_class(
            cloneable,
            ClassDef {
                name: "java.lang.Cloneable".to_string(),
                kind: ClassKind::Interface,
                type_params: vec![],
                super_class: None,
                interfaces: vec![],
                fields: vec![],
                constructors: vec![],
                methods: vec![],
            },
        );
        store.define_class(
            serializable,
            ClassDef {
                name: "java.io.Serializable".to_string(),
                kind: ClassKind::Interface,
                type_params: vec![],
                super_class: None,
                interfaces: vec![],
                fields: vec![],
                constructors: vec![],
                methods: vec![],
            },
        );

        store.well_known = Some(WellKnownTypes {
            object,
            string,
            integer,
            cloneable,
            serializable,
        });

        store
    }
}

/// Binary names of the classes materialized by [`TypeStore::with_minimal_jdk`].
///
/// This list is the single source of truth for Nova's "minimal JDK" model and is
/// used by:
/// - `nova-types` to eagerly reserve stable [`ClassId`]s before defining the
///   placeholder class bodies, and
/// - `nova-db`'s workspace loader to seed stable, host-managed `ClassId`
///   assignments for these well-known JDK types.
///
/// The set is intentionally small and **must** stay in sync with
/// `TypeStore::with_minimal_jdk()` (the implementation and the list are checked
/// by tests).
pub const MINIMAL_JDK_BINARY_NAMES: &[&str] = &[
    // java.lang
    "java.lang.Object",
    "java.lang.Throwable",
    "java.lang.String",
    "java.lang.Integer",
    "java.lang.Number",
    "java.lang.Math",
    "java.lang.Boolean",
    "java.lang.Byte",
    "java.lang.Short",
    "java.lang.Character",
    "java.lang.Long",
    "java.lang.Float",
    "java.lang.Double",
    "java.lang.Cloneable",
    "java.lang.Runnable",
    "java.lang.Iterable",
    "java.lang.Class",
    "java.lang.System",
    // java.io
    "java.io.Serializable",
    "java.io.PrintStream",
    // java.util
    "java.util.List",
    "java.util.Collections",
    "java.util.ArrayList",
    // java.util.function
    "java.util.function.Function",
    "java.util.function.Supplier",
    "java.util.function.Consumer",
    "java.util.function.Predicate",
];

impl TypeStore {
    pub fn with_minimal_jdk() -> Self {
        let mut store = TypeStore::default();

        // Reserve stable ids for all minimal JDK types up-front.
        for &name in MINIMAL_JDK_BINARY_NAMES {
            store.intern_class_id(name);
        }

        // java.lang
        let object = store
            .lookup_class("java.lang.Object")
            .expect("minimal JDK must contain java.lang.Object");
        let throwable = store
            .lookup_class("java.lang.Throwable")
            .expect("minimal JDK must contain java.lang.Throwable");
        let string = store
            .lookup_class("java.lang.String")
            .expect("minimal JDK must contain java.lang.String");
        let integer = store
            .lookup_class("java.lang.Integer")
            .expect("minimal JDK must contain java.lang.Integer");
        let cloneable = store
            .lookup_class("java.lang.Cloneable")
            .expect("minimal JDK must contain java.lang.Cloneable");
        let serializable = store
            .lookup_class("java.io.Serializable")
            .expect("minimal JDK must contain java.io.Serializable");

        let object_ty = Type::class(object, vec![]);
        let string_ty = Type::class(string, vec![]);
        store.define_class(
            object,
            ClassDef {
                name: "java.lang.Object".to_string(),
                kind: ClassKind::Class,
                type_params: vec![],
                super_class: None,
                interfaces: vec![],
                fields: vec![],
                constructors: vec![ConstructorDef {
                    params: vec![],
                    is_varargs: false,
                    is_accessible: true,
                }],
                methods: vec![
                    MethodDef {
                        name: "toString".to_string(),
                        type_params: vec![],
                        params: vec![],
                        return_type: string_ty.clone(),
                        is_static: false,
                        is_varargs: false,
                        is_abstract: false,
                    },
                    MethodDef {
                        name: "equals".to_string(),
                        type_params: vec![],
                        params: vec![object_ty.clone()],
                        return_type: Type::Primitive(PrimitiveType::Boolean),
                        is_static: false,
                        is_varargs: false,
                        is_abstract: false,
                    },
                    MethodDef {
                        name: "hashCode".to_string(),
                        type_params: vec![],
                        params: vec![],
                        return_type: Type::Primitive(PrimitiveType::Int),
                        is_static: false,
                        is_varargs: false,
                        is_abstract: false,
                    },
                ],
            },
        );
        store.define_class(
            throwable,
            ClassDef {
                name: "java.lang.Throwable".to_string(),
                kind: ClassKind::Class,
                type_params: vec![],
                super_class: Some(object_ty.clone()),
                interfaces: vec![],
                fields: vec![],
                constructors: vec![],
                methods: vec![],
            },
        );
        store.define_class(
            string,
            ClassDef {
                name: "java.lang.String".to_string(),
                kind: ClassKind::Class,
                type_params: vec![],
                super_class: Some(object_ty.clone()),
                interfaces: vec![],
                fields: vec![],
                constructors: vec![],
                methods: vec![],
            },
        );
        if let Some(string_def) = store.class_mut(string) {
            let string_ty = Type::class(string, vec![]);
            string_def.methods = vec![
                MethodDef {
                    name: "length".to_string(),
                    type_params: vec![],
                    params: vec![],
                    return_type: Type::Primitive(PrimitiveType::Int),
                    is_static: false,
                    is_varargs: false,
                    is_abstract: false,
                },
                MethodDef {
                    name: "substring".to_string(),
                    type_params: vec![],
                    params: vec![Type::Primitive(PrimitiveType::Int)],
                    return_type: string_ty.clone(),
                    is_static: false,
                    is_varargs: false,
                    is_abstract: false,
                },
                MethodDef {
                    name: "substring".to_string(),
                    type_params: vec![],
                    params: vec![
                        Type::Primitive(PrimitiveType::Int),
                        Type::Primitive(PrimitiveType::Int),
                    ],
                    return_type: string_ty.clone(),
                    is_static: false,
                    is_varargs: false,
                    is_abstract: false,
                },
                MethodDef {
                    name: "charAt".to_string(),
                    type_params: vec![],
                    params: vec![Type::Primitive(PrimitiveType::Int)],
                    return_type: Type::Primitive(PrimitiveType::Char),
                    is_static: false,
                    is_varargs: false,
                    is_abstract: false,
                },
                MethodDef {
                    name: "isEmpty".to_string(),
                    type_params: vec![],
                    params: vec![],
                    return_type: Type::Primitive(PrimitiveType::Boolean),
                    is_static: false,
                    is_varargs: false,
                    is_abstract: false,
                },
                MethodDef {
                    name: "valueOf".to_string(),
                    type_params: vec![],
                    params: vec![Type::Primitive(PrimitiveType::Int)],
                    return_type: string_ty,
                    is_static: true,
                    is_varargs: false,
                    is_abstract: false,
                },
            ];
        }
        let number = store
            .lookup_class("java.lang.Number")
            .expect("minimal JDK must contain java.lang.Number");
        store.define_class(
            number,
            ClassDef {
                name: "java.lang.Number".to_string(),
                kind: ClassKind::Class,
                type_params: vec![],
                super_class: Some(Type::class(object, vec![])),
                interfaces: vec![],
                fields: vec![],
                constructors: vec![],
                methods: vec![],
            },
        );

        let math = store
            .lookup_class("java.lang.Math")
            .expect("minimal JDK must contain java.lang.Math");
        store.define_class(
            math,
            ClassDef {
                name: "java.lang.Math".to_string(),
                kind: ClassKind::Class,
                type_params: vec![],
                super_class: Some(Type::class(object, vec![])),
                interfaces: vec![],
                fields: vec![
                    FieldDef {
                        name: "PI".to_string(),
                        ty: Type::Primitive(PrimitiveType::Double),
                        is_static: true,
                        is_final: true,
                    },
                    FieldDef {
                        name: "E".to_string(),
                        ty: Type::Primitive(PrimitiveType::Double),
                        is_static: true,
                        is_final: true,
                    },
                ],
                constructors: vec![],
                methods: vec![
                    MethodDef {
                        name: "max".to_string(),
                        type_params: vec![],
                        params: vec![
                            Type::Primitive(PrimitiveType::Int),
                            Type::Primitive(PrimitiveType::Int),
                        ],
                        return_type: Type::Primitive(PrimitiveType::Int),
                        is_static: true,
                        is_varargs: false,
                        is_abstract: false,
                    },
                    MethodDef {
                        name: "max".to_string(),
                        type_params: vec![],
                        params: vec![
                            Type::Primitive(PrimitiveType::Long),
                            Type::Primitive(PrimitiveType::Long),
                        ],
                        return_type: Type::Primitive(PrimitiveType::Long),
                        is_static: true,
                        is_varargs: false,
                        is_abstract: false,
                    },
                    MethodDef {
                        name: "max".to_string(),
                        type_params: vec![],
                        params: vec![
                            Type::Primitive(PrimitiveType::Float),
                            Type::Primitive(PrimitiveType::Float),
                        ],
                        return_type: Type::Primitive(PrimitiveType::Float),
                        is_static: true,
                        is_varargs: false,
                        is_abstract: false,
                    },
                    MethodDef {
                        name: "max".to_string(),
                        type_params: vec![],
                        params: vec![
                            Type::Primitive(PrimitiveType::Double),
                            Type::Primitive(PrimitiveType::Double),
                        ],
                        return_type: Type::Primitive(PrimitiveType::Double),
                        is_static: true,
                        is_varargs: false,
                        is_abstract: false,
                    },
                    MethodDef {
                        name: "min".to_string(),
                        type_params: vec![],
                        params: vec![
                            Type::Primitive(PrimitiveType::Int),
                            Type::Primitive(PrimitiveType::Int),
                        ],
                        return_type: Type::Primitive(PrimitiveType::Int),
                        is_static: true,
                        is_varargs: false,
                        is_abstract: false,
                    },
                    MethodDef {
                        name: "min".to_string(),
                        type_params: vec![],
                        params: vec![
                            Type::Primitive(PrimitiveType::Long),
                            Type::Primitive(PrimitiveType::Long),
                        ],
                        return_type: Type::Primitive(PrimitiveType::Long),
                        is_static: true,
                        is_varargs: false,
                        is_abstract: false,
                    },
                    MethodDef {
                        name: "min".to_string(),
                        type_params: vec![],
                        params: vec![
                            Type::Primitive(PrimitiveType::Float),
                            Type::Primitive(PrimitiveType::Float),
                        ],
                        return_type: Type::Primitive(PrimitiveType::Float),
                        is_static: true,
                        is_varargs: false,
                        is_abstract: false,
                    },
                    MethodDef {
                        name: "min".to_string(),
                        type_params: vec![],
                        params: vec![
                            Type::Primitive(PrimitiveType::Double),
                            Type::Primitive(PrimitiveType::Double),
                        ],
                        return_type: Type::Primitive(PrimitiveType::Double),
                        is_static: true,
                        is_varargs: false,
                        is_abstract: false,
                    },
                ],
            },
        );

        let boolean = store
            .lookup_class("java.lang.Boolean")
            .expect("minimal JDK must contain java.lang.Boolean");
        store.define_class(
            boolean,
            ClassDef {
                name: "java.lang.Boolean".to_string(),
                kind: ClassKind::Class,
                type_params: vec![],
                super_class: Some(Type::class(object, vec![])),
                interfaces: vec![],
                fields: vec![],
                constructors: vec![],
                methods: vec![],
            },
        );

        let byte = store
            .lookup_class("java.lang.Byte")
            .expect("minimal JDK must contain java.lang.Byte");
        store.define_class(
            byte,
            ClassDef {
                name: "java.lang.Byte".to_string(),
                kind: ClassKind::Class,
                type_params: vec![],
                super_class: Some(Type::class(number, vec![])),
                interfaces: vec![],
                fields: vec![],
                constructors: vec![],
                methods: vec![],
            },
        );

        let short = store
            .lookup_class("java.lang.Short")
            .expect("minimal JDK must contain java.lang.Short");
        store.define_class(
            short,
            ClassDef {
                name: "java.lang.Short".to_string(),
                kind: ClassKind::Class,
                type_params: vec![],
                super_class: Some(Type::class(number, vec![])),
                interfaces: vec![],
                fields: vec![],
                constructors: vec![],
                methods: vec![],
            },
        );

        let character = store
            .lookup_class("java.lang.Character")
            .expect("minimal JDK must contain java.lang.Character");
        store.define_class(
            character,
            ClassDef {
                name: "java.lang.Character".to_string(),
                kind: ClassKind::Class,
                type_params: vec![],
                super_class: Some(Type::class(object, vec![])),
                interfaces: vec![],
                fields: vec![],
                constructors: vec![],
                methods: vec![],
            },
        );
        store.define_class(
            integer,
            ClassDef {
                name: "java.lang.Integer".to_string(),
                kind: ClassKind::Class,
                type_params: vec![],
                super_class: Some(Type::class(number, vec![])),
                interfaces: vec![],
                fields: vec![],
                constructors: vec![],
                methods: vec![],
            },
        );
        let long = store
            .lookup_class("java.lang.Long")
            .expect("minimal JDK must contain java.lang.Long");
        store.define_class(
            long,
            ClassDef {
                name: "java.lang.Long".to_string(),
                kind: ClassKind::Class,
                type_params: vec![],
                super_class: Some(Type::class(number, vec![])),
                interfaces: vec![],
                fields: vec![],
                constructors: vec![],
                methods: vec![],
            },
        );
        let float = store
            .lookup_class("java.lang.Float")
            .expect("minimal JDK must contain java.lang.Float");
        store.define_class(
            float,
            ClassDef {
                name: "java.lang.Float".to_string(),
                kind: ClassKind::Class,
                type_params: vec![],
                super_class: Some(Type::class(number, vec![])),
                interfaces: vec![],
                fields: vec![],
                constructors: vec![],
                methods: vec![],
            },
        );
        let double = store
            .lookup_class("java.lang.Double")
            .expect("minimal JDK must contain java.lang.Double");
        store.define_class(
            double,
            ClassDef {
                name: "java.lang.Double".to_string(),
                kind: ClassKind::Class,
                type_params: vec![],
                super_class: Some(Type::class(number, vec![])),
                interfaces: vec![],
                fields: vec![],
                constructors: vec![],
                methods: vec![],
            },
        );
        store.define_class(
            cloneable,
            ClassDef {
                name: "java.lang.Cloneable".to_string(),
                kind: ClassKind::Interface,
                type_params: vec![],
                super_class: Some(Type::class(object, vec![])),
                interfaces: vec![],
                fields: vec![],
                constructors: vec![],
                methods: vec![],
            },
        );
        store.define_class(
            serializable,
            ClassDef {
                name: "java.io.Serializable".to_string(),
                kind: ClassKind::Interface,
                type_params: vec![],
                super_class: Some(Type::class(object, vec![])),
                interfaces: vec![],
                fields: vec![],
                constructors: vec![],
                methods: vec![],
            },
        );

        // java.lang.Runnable
        let runnable = store
            .lookup_class("java.lang.Runnable")
            .expect("minimal JDK must contain java.lang.Runnable");
        store.define_class(
            runnable,
            ClassDef {
                name: "java.lang.Runnable".to_string(),
                kind: ClassKind::Interface,
                type_params: vec![],
                super_class: Some(Type::class(object, vec![])),
                interfaces: vec![],
                fields: vec![],
                constructors: vec![],
                methods: vec![MethodDef {
                    name: "run".to_string(),
                    type_params: vec![],
                    params: vec![],
                    return_type: Type::Void,
                    is_static: false,
                    is_varargs: false,
                    is_abstract: true,
                }],
            },
        );

        // java.lang.Iterable<T>
        //
        // This is used by richer typeck tests (e.g. foreach element inference)
        // without requiring a full on-disk JDK model.
        let iterable_t = store.add_type_param("T", vec![Type::class(object, vec![])]);
        let iterable = store
            .lookup_class("java.lang.Iterable")
            .expect("minimal JDK must contain java.lang.Iterable");
        store.define_class(
            iterable,
            ClassDef {
                name: "java.lang.Iterable".to_string(),
                kind: ClassKind::Interface,
                type_params: vec![iterable_t],
                super_class: Some(Type::class(object, vec![])),
                interfaces: vec![],
                fields: vec![],
                constructors: vec![],
                methods: vec![],
            },
        );

        // java.io.PrintStream
        let print_stream = store
            .lookup_class("java.io.PrintStream")
            .expect("minimal JDK must contain java.io.PrintStream");
        store.define_class(
            print_stream,
            ClassDef {
                name: "java.io.PrintStream".to_string(),
                kind: ClassKind::Class,
                type_params: vec![],
                super_class: Some(Type::class(object, vec![])),
                interfaces: vec![],
                fields: vec![],
                constructors: vec![],
                methods: vec![
                    MethodDef {
                        name: "println".to_string(),
                        type_params: vec![],
                        params: vec![Type::class(string, vec![])],
                        return_type: Type::Void,
                        is_static: false,
                        is_varargs: false,
                        is_abstract: false,
                    },
                    MethodDef {
                        name: "println".to_string(),
                        type_params: vec![],
                        params: vec![Type::Primitive(PrimitiveType::Int)],
                        return_type: Type::Void,
                        is_static: false,
                        is_varargs: false,
                        is_abstract: false,
                    },
                ],
            },
        );

        // java.lang.System
        let system = store
            .lookup_class("java.lang.System")
            .expect("minimal JDK must contain java.lang.System");
        store.define_class(
            system,
            ClassDef {
                name: "java.lang.System".to_string(),
                kind: ClassKind::Class,
                type_params: vec![],
                super_class: Some(Type::class(object, vec![])),
                interfaces: vec![],
                fields: vec![FieldDef {
                    name: "out".to_string(),
                    ty: Type::class(print_stream, vec![]),
                    is_static: true,
                    is_final: true,
                }],
                constructors: vec![],
                methods: vec![],
            },
        );

        // java.util.List<E>
        let list_e = store.add_type_param("E", vec![Type::class(object, vec![])]);
        let list = store
            .lookup_class("java.util.List")
            .expect("minimal JDK must contain java.util.List");
        store.define_class(
            list,
            ClassDef {
                name: "java.util.List".to_string(),
                kind: ClassKind::Interface,
                type_params: vec![list_e],
                super_class: Some(Type::class(object, vec![])),
                interfaces: vec![Type::class(iterable, vec![Type::TypeVar(list_e)])],
                fields: vec![],
                constructors: vec![],
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
            },
        );

        // java.util.Collections
        //
        // We include this primarily to support target-typing regression tests like:
        // `return Collections.emptyList();` where the method has no arguments and
        // type argument inference depends on the expected return type.
        let collections_t = store.add_type_param("T", vec![Type::class(object, vec![])]);
        let collections_u = store.add_type_param("U", vec![Type::class(object, vec![])]);
        let collections = store
            .lookup_class("java.util.Collections")
            .expect("minimal JDK must contain java.util.Collections");
        store.define_class(
            collections,
            ClassDef {
                name: "java.util.Collections".to_string(),
                kind: ClassKind::Class,
                type_params: vec![],
                super_class: Some(Type::class(object, vec![])),
                interfaces: vec![],
                fields: vec![],
                constructors: vec![],
                methods: vec![
                    MethodDef {
                        name: "emptyList".to_string(),
                        type_params: vec![collections_t],
                        params: vec![],
                        return_type: Type::class(list, vec![Type::TypeVar(collections_t)]),
                        is_static: true,
                        is_varargs: false,
                        is_abstract: false,
                    },
                    MethodDef {
                        name: "singletonList".to_string(),
                        type_params: vec![collections_u],
                        params: vec![Type::TypeVar(collections_u)],
                        return_type: Type::class(list, vec![Type::TypeVar(collections_u)]),
                        is_static: true,
                        is_varargs: false,
                        is_abstract: false,
                    },
                ],
            },
        );

        // java.util.ArrayList<E> implements List<E>
        let array_list_e = store.add_type_param("E", vec![Type::class(object, vec![])]);
        let array_list = store
            .lookup_class("java.util.ArrayList")
            .expect("minimal JDK must contain java.util.ArrayList");
        store.define_class(
            array_list,
            ClassDef {
                name: "java.util.ArrayList".to_string(),
                kind: ClassKind::Class,
                type_params: vec![array_list_e],
                super_class: Some(Type::class(object, vec![])),
                interfaces: vec![Type::class(list, vec![Type::TypeVar(array_list_e)])],
                fields: vec![],
                constructors: vec![ConstructorDef {
                    params: vec![],
                    is_varargs: false,
                    is_accessible: true,
                }],
                methods: vec![],
            },
        );

        // java.util.function.Function<T, R>
        let function_t = store.add_type_param("T", vec![Type::class(object, vec![])]);
        let function_r = store.add_type_param("R", vec![Type::class(object, vec![])]);
        let function = store
            .lookup_class("java.util.function.Function")
            .expect("minimal JDK must contain java.util.function.Function");
        store.define_class(
            function,
            ClassDef {
                name: "java.util.function.Function".to_string(),
                kind: ClassKind::Interface,
                type_params: vec![function_t, function_r],
                super_class: Some(Type::class(object, vec![])),
                interfaces: vec![],
                fields: vec![],
                constructors: vec![],
                methods: vec![MethodDef {
                    name: "apply".to_string(),
                    type_params: vec![],
                    params: vec![Type::TypeVar(function_t)],
                    return_type: Type::TypeVar(function_r),
                    is_static: false,
                    is_varargs: false,
                    is_abstract: true,
                }],
            },
        );

        // java.util.function.Supplier<T>
        let supplier_t = store.add_type_param("T", vec![Type::class(object, vec![])]);
        let supplier = store
            .lookup_class("java.util.function.Supplier")
            .expect("minimal JDK must contain java.util.function.Supplier");
        store.define_class(
            supplier,
            ClassDef {
                name: "java.util.function.Supplier".to_string(),
                kind: ClassKind::Interface,
                type_params: vec![supplier_t],
                super_class: Some(Type::class(object, vec![])),
                interfaces: vec![],
                fields: vec![],
                constructors: vec![],
                methods: vec![MethodDef {
                    name: "get".to_string(),
                    type_params: vec![],
                    params: vec![],
                    return_type: Type::TypeVar(supplier_t),
                    is_static: false,
                    is_varargs: false,
                    is_abstract: true,
                }],
            },
        );

        // java.util.function.Consumer<T>
        let consumer_t = store.add_type_param("T", vec![Type::class(object, vec![])]);
        let consumer = store
            .lookup_class("java.util.function.Consumer")
            .expect("minimal JDK must contain java.util.function.Consumer");
        store.define_class(
            consumer,
            ClassDef {
                name: "java.util.function.Consumer".to_string(),
                kind: ClassKind::Interface,
                type_params: vec![consumer_t],
                super_class: Some(Type::class(object, vec![])),
                interfaces: vec![],
                fields: vec![],
                constructors: vec![],
                methods: vec![MethodDef {
                    name: "accept".to_string(),
                    type_params: vec![],
                    params: vec![Type::TypeVar(consumer_t)],
                    return_type: Type::Void,
                    is_static: false,
                    is_varargs: false,
                    is_abstract: true,
                }],
            },
        );

        // java.util.function.Predicate<T>
        let predicate_t = store.add_type_param("T", vec![Type::class(object, vec![])]);
        let predicate = store
            .lookup_class("java.util.function.Predicate")
            .expect("minimal JDK must contain java.util.function.Predicate");
        store.define_class(
            predicate,
            ClassDef {
                name: "java.util.function.Predicate".to_string(),
                kind: ClassKind::Interface,
                type_params: vec![predicate_t],
                super_class: Some(Type::class(object, vec![])),
                interfaces: vec![],
                fields: vec![],
                constructors: vec![],
                methods: vec![MethodDef {
                    name: "test".to_string(),
                    type_params: vec![],
                    params: vec![Type::TypeVar(predicate_t)],
                    return_type: Type::Primitive(PrimitiveType::Boolean),
                    is_static: false,
                    is_varargs: false,
                    is_abstract: true,
                }],
            },
        );

        // java.lang.Class<T>
        let class_t = store.add_type_param("T", vec![Type::class(object, vec![])]);
        let class = store
            .lookup_class("java.lang.Class")
            .expect("minimal JDK must contain java.lang.Class");
        store.define_class(
            class,
            ClassDef {
                name: "java.lang.Class".to_string(),
                kind: ClassKind::Class,
                type_params: vec![class_t],
                super_class: Some(Type::class(object, vec![])),
                interfaces: vec![],
                fields: vec![],
                constructors: vec![],
                methods: vec![],
            },
        );

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
            store.upsert_class(class);
        }
        store
    }

    /// Returns the number of type parameters currently stored in this `TypeStore`.
    ///
    /// `TypeVarId`s are allocated densely starting at zero, so this can be used to
    /// predict the next `TypeVarId` before allocating a batch of parameters.
    pub fn type_param_count(&self) -> usize {
        self.type_params.len()
    }

    pub fn add_type_param(
        &mut self,
        name: impl Into<String>,
        upper_bounds: Vec<Type>,
    ) -> TypeVarId {
        let id = TypeVarId(self.type_params.len() as u32);
        self.type_params.push(TypeParamDef {
            name: name.into(),
            upper_bounds,
            lower_bound: None,
        });
        id
    }

    /// Overwrite the existing type parameter definition at `id`.
    ///
    /// This is useful for external type loaders that need to allocate `TypeVarId`s
    /// up-front (to support self-referential bounds like `T extends Comparable<T>`)
    /// and then fill in the final bounds once all type variables are in scope.
    ///
    /// # Panics
    ///
    /// Panics if `id` is out of bounds, or if `def.name` does not match the name
    /// originally associated with `id`.
    pub fn define_type_param(&mut self, id: TypeVarId, def: TypeParamDef) {
        let slot = self
            .type_params
            .get_mut(id.0 as usize)
            .unwrap_or_else(|| panic!("define_type_param: invalid TypeVarId {:?}", id));
        let expected_name = slot.name.clone();

        assert!(
            def.name == expected_name,
            "define_type_param: attempted to define {:?} with name {:?}, but id is reserved for {:?}",
            id,
            def.name,
            expected_name
        );

        *slot = def;
    }

    /// Reserve (or reuse) a stable [`ClassId`] for `binary_name`.
    ///
    /// External type loaders (e.g. reading `.class` files or JDK stubs) often need a
    /// stable id for a class *before* they have parsed its full body. This enables:
    ///
    /// - Building cyclic graphs (mutually-referential types) without infinite recursion.
    /// - Interning ids early while loading referenced super classes / interfaces.
    ///
    /// If the class has not been seen before, this inserts a conservative placeholder
    /// [`ClassDef`] (kind = [`ClassKind::Class`], no supertypes, no fields, no constructors,
    /// no methods, no type params) and returns its id. If it already exists, returns the
    /// existing id.
    pub fn intern_class_id(&mut self, binary_name: &str) -> ClassId {
        if let Some(id) = self.class_by_name.get(binary_name).copied() {
            return id;
        }

        if let Some(id) = self.tombstones.remove(binary_name) {
            self.class_by_name.insert(binary_name.to_string(), id);
            return id;
        }

        self.add_class(ClassDef {
            name: binary_name.to_string(),
            kind: ClassKind::Class,
            type_params: vec![],
            super_class: None,
            interfaces: vec![],
            fields: vec![],
            constructors: vec![],
            methods: vec![],
        })
    }

    /// Overwrite the existing class definition at `id`.
    ///
    /// This is intended to pair with [`TypeStore::intern_class_id`]: reserve ids for
    /// a set of binary names first, then later populate/replace those placeholders
    /// with fully parsed class definitions.
    ///
    /// # Panics
    ///
    /// Panics if `id` is out of bounds, or if `def.name` does not match the name
    /// originally associated with `id`.
    pub fn define_class(&mut self, id: ClassId, def: ClassDef) {
        let slot = self
            .classes
            .get_mut(id.to_raw() as usize)
            .unwrap_or_else(|| panic!("define_class: invalid ClassId {:?}", id));
        let expected_name = slot.name.clone();

        assert!(
            def.name == expected_name,
            "define_class: attempted to define {:?} with name {:?}, but id is reserved for {:?}",
            id,
            def.name,
            expected_name
        );
        assert!(
            self.class_by_name.get(&expected_name).copied() == Some(id),
            "define_class: TypeStore invariant violation: class_by_name[{:?}] did not point at {:?}",
            expected_name,
            id
        );

        *slot = def;
    }
    pub fn add_class(&mut self, def: ClassDef) -> ClassId {
        let id = ClassId::from_raw(self.classes.len() as u32);
        if self.class_by_name.contains_key(&def.name) || self.tombstones.contains_key(&def.name) {
            // Avoid silently creating two ids for the same class.
            // This is a programmer error in tests/builders.
            panic!("duplicate class definition for {}", def.name);
        }
        self.class_by_name.insert(def.name.clone(), id);
        self.classes.push(def);
        id
    }

    /// Insert or replace a class definition.
    ///
    /// This is primarily used for incremental updates where types may originate
    /// from multiple sources (classpath stubs, source code, generated overlays).
    /// The `ClassId` is stable for a given binary name as long as the store lives.
    pub fn upsert_class(&mut self, def: ClassDef) -> ClassId {
        if let Some(id) = self.class_by_name.get(&def.name).copied() {
            self.define_class(id, def);
            return id;
        }

        if let Some(id) = self.tombstones.remove(&def.name) {
            self.class_by_name.insert(def.name.clone(), id);
            self.define_class(id, def);
            return id;
        }

        self.add_class(def)
    }

    /// Remove a class by binary name.
    ///
    /// The removed slot is kept as an inert placeholder so existing `ClassId`s
    /// remain stable. Lookups by name will no longer find the class until it is
    /// re-inserted via [`TypeStore::upsert_class`].
    pub fn remove_class(&mut self, name: &str) -> Option<ClassId> {
        let id = self.class_by_name.remove(name)?;
        self.tombstones.insert(name.to_string(), id);

        if let Some(class_def) = self.classes.get_mut(id.to_raw() as usize) {
            class_def.type_params.clear();
            class_def.interfaces.clear();
            class_def.fields.clear();
            class_def.constructors.clear();
            class_def.methods.clear();

            // Ensure basic subtyping queries still behave sensibly for stale
            // references to a deleted class.
            match class_def.kind {
                ClassKind::Interface => class_def.super_class = None,
                ClassKind::Class => {
                    class_def.super_class = self
                        .well_known
                        .as_ref()
                        .map(|wk| Type::class(wk.object, vec![]))
                }
            }
        }

        Some(id)
    }
    pub fn class_id(&self, name: &str) -> Option<ClassId> {
        self.lookup_class(name)
    }

    /// Iterate over all class definitions currently stored in this [`TypeStore`].
    ///
    /// This is primarily intended for IDE features (e.g. completion) that need to
    /// search across known types without maintaining a separate index.
    ///
    /// Note: The iterator includes inert placeholder/tombstone entries. Callers
    /// should be prepared to filter out classes that are not relevant.
    pub fn iter_classes(&self) -> impl Iterator<Item = (ClassId, &ClassDef)> {
        self.classes
            .iter()
            .enumerate()
            .map(|(idx, def)| (ClassId::from_raw(idx as u32), def))
    }

    pub fn class_mut(&mut self, id: ClassId) -> Option<&mut ClassDef> {
        self.classes.get_mut(id.to_raw() as usize)
    }
}

impl TypeEnv for TypeStore {
    fn class(&self, id: ClassId) -> Option<&ClassDef> {
        self.classes.get(id.to_raw() as usize)
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
            .expect("TypeStore must initialize well-known types")
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

        // Every class/interface type is a subtype of `Object` (JLS 4.10.2).
        (Type::Class(_), Type::Class(ClassType { def, .. })) if *def == env.well_known().object => {
            true
        }

        // `X <: (A & B)` iff `X <: A` and `X <: B`.
        //
        // Note: handle this before the `(A & B) <: X` case so that intersection-to-intersection
        // subtyping works as expected:
        //   (A & B) <: (C & D) iff (A & B) <: C and (A & B) <: D
        (other, Type::Intersection(types)) => types.iter().all(|t| is_subtype(env, other, t)),

        // `(A & B) <: X` iff `A <: X` or `B <: X`.
        (Type::Intersection(types), other) => types.iter().any(|t| is_subtype(env, t, other)),

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
    matches!(
        (from, to),
        (Byte, Short | Int | Long | Float | Double)
            | (Short, Int | Long | Float | Double)
            | (Char, Int | Long | Float | Double)
            | (Int, Long | Float | Double)
            | (Long, Float | Double)
            | (Float, Double)
    )
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

    while let Some(mut current) = queue.pop_front() {
        // Allow supertypes to be recorded as `Type::Named` (common for source-derived
        // environments where referenced types may not have been interned yet).
        if let Type::Named(name) = &current {
            if let Some(id) = env.lookup_class(name) {
                current = Type::class(id, vec![]);
            }
        }

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
        // In Java, every interface implicitly has `Object` as a supertype (JLS 4.10.2).
        if class_def.kind == ClassKind::Interface {
            queue.push_back(Type::class(env.well_known().object, vec![]));
        }
    }

    false
}

fn type_args_compatible(env: &dyn TypeEnv, def: ClassId, sub: &[Type], super_: &[Type]) -> bool {
    let type_param_len = env.class(def).map(|c| c.type_params.len()).unwrap_or(0);
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
    for (actual, formal) in sub.iter().zip(super_) {
        if !type_arg_contained_by(env, actual, formal) {
            return false;
        }
    }
    true
}

/// Type argument containment (JLS 4.5.1 / 4.10.2).
///
/// This is the relation used when comparing two parameterized types with the same
/// generic class/interface, e.g. `List<? extends String> <: List<? extends Object>`.
fn type_arg_contained_by(env: &dyn TypeEnv, actual: &Type, formal: &Type) -> bool {
    match formal {
        // `?` contains any type argument.
        Type::Wildcard(WildcardBound::Unbounded) => true,

        // `? extends U` contains:
        // * `A` if `A <: U`
        // * `? extends S` if `S <: U`
        // * `?` as shorthand for `? extends Object` (so only if `Object <: U`)
        Type::Wildcard(WildcardBound::Extends(upper)) => match actual {
            Type::Wildcard(WildcardBound::Unbounded) => {
                let object = Type::class(env.well_known().object, vec![]);
                is_subtype(env, &object, upper)
            }
            Type::Wildcard(WildcardBound::Extends(actual_upper)) => {
                is_subtype(env, actual_upper, upper)
            }
            Type::Wildcard(WildcardBound::Super(_)) => false,
            other => is_subtype(env, other, upper),
        },

        // `? super L` contains:
        // * `A` if `L <: A`
        // * `? super S` if `L <: S` (contravariant containment)
        Type::Wildcard(WildcardBound::Super(lower)) => match actual {
            Type::Wildcard(WildcardBound::Super(actual_lower)) => {
                is_subtype(env, lower, actual_lower)
            }
            Type::Wildcard(_) => false,
            other => is_subtype(env, lower, other),
        },

        // Non-wildcard type arguments are invariant.
        _ => actual == formal,
    }
}

fn substitute(ty: &Type, subst: &HashMap<TypeVarId, Type>) -> Type {
    match ty {
        Type::TypeVar(id) => subst.get(id).cloned().unwrap_or(Type::TypeVar(*id)),
        Type::Array(elem) => Type::Array(Box::new(substitute(elem, subst))),
        Type::Class(ClassType { def, args }) => {
            Type::class(*def, args.iter().map(|a| substitute(a, subst)).collect())
        }
        Type::Wildcard(WildcardBound::Unbounded) => Type::Wildcard(WildcardBound::Unbounded),
        Type::Wildcard(WildcardBound::Extends(upper)) => {
            Type::Wildcard(WildcardBound::Extends(Box::new(substitute(upper, subst))))
        }
        Type::Wildcard(WildcardBound::Super(lower)) => {
            Type::Wildcard(WildcardBound::Super(Box::new(substitute(lower, subst))))
        }
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

/// Compile-time constant value used by conversions.
///
/// This intentionally only models the small subset of constants needed by the
/// conversion engine (notably JLS 5.2 constant narrowing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstValue {
    /// Integral constant value (`byte`, `short`, `char`, `int`, `long`).
    Int(i64),
    /// Boolean constant value.
    Boolean(bool),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UncheckedReason {
    RawConversion,
    UncheckedCast,
    UncheckedVarargs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeWarning {
    Unchecked(UncheckedReason),
    /// A static member was accessed via an instance expression (e.g. `obj.f()`).
    ///
    /// Java allows this but compilers typically warn because it is misleading.
    StaticAccessViaInstance,
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
                conv.warnings
                    .push(TypeWarning::Unchecked(UncheckedReason::RawConversion));
            }
            Some(conv)
        }
        _ => None,
    }
}

/// Method invocation conversion (JLS 5.3): strict conversion plus boxing,
/// unboxing, and unchecked raw conversions.
pub fn method_invocation_conversion(
    env: &dyn TypeEnv,
    from: &Type,
    to: &Type,
) -> Option<Conversion> {
    let from = canonicalize_named(env, from);
    let to = canonicalize_named(env, to);

    // Error recovery: treat unknown/error types as compatible with anything to avoid cascading
    // resolution failures (e.g. overload resolution during IDE completion when the active argument
    // expression is still empty).
    if from.is_errorish() || to.is_errorish() {
        return Some(Conversion::new(ConversionStep::Identity));
    }

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
                let mut conv = Conversion::new(ConversionStep::Boxing)
                    .push_step(ConversionStep::WideningReference);
                if raw_warning(env, &boxed, &to) {
                    conv.warnings
                        .push(TypeWarning::Unchecked(UncheckedReason::RawConversion));
                }
                return Some(conv);
            }
        }

        // Widening primitive conversion followed by boxing (e.g. `int` -> `long` -> `Long`).
        if to.is_reference() {
            let numeric_targets = [
                PrimitiveType::Byte,
                PrimitiveType::Short,
                PrimitiveType::Char,
                PrimitiveType::Int,
                PrimitiveType::Long,
                PrimitiveType::Float,
                PrimitiveType::Double,
            ];
            for widened in numeric_targets {
                if widened == p {
                    continue;
                }
                if !primitive_widening(p, widened) {
                    continue;
                }
                let Some(boxed) = boxing_type(env, widened) else {
                    continue;
                };
                if boxed == to {
                    return Some(
                        Conversion::new(ConversionStep::WideningPrimitive)
                            .push_step(ConversionStep::Boxing),
                    );
                }
                if boxed.is_reference() && is_subtype(env, &boxed, &to) {
                    let mut conv = Conversion::new(ConversionStep::WideningPrimitive)
                        .push_step(ConversionStep::Boxing)
                        .push_step(ConversionStep::WideningReference);
                    if raw_warning(env, &boxed, &to) {
                        conv.warnings
                            .push(TypeWarning::Unchecked(UncheckedReason::RawConversion));
                    }
                    return Some(conv);
                }
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
                    Conversion::new(ConversionStep::Unboxing)
                        .push_step(ConversionStep::WideningPrimitive),
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
    assignment_conversion_with_const(env, from, to, None)
}

/// Assignment conversion (JLS 5.2) with an optional compile-time constant value.
///
/// This extends [`method_invocation_conversion`] with *constant narrowing*
/// conversions (e.g. `byte b = 1;`).
pub fn assignment_conversion_with_const(
    env: &dyn TypeEnv,
    from: &Type,
    to: &Type,
    const_value: Option<ConstValue>,
) -> Option<Conversion> {
    if let Some(conv) = method_invocation_conversion(env, from, to) {
        return Some(conv);
    }

    constant_narrowing_conversion(env, from, to, const_value)
}

fn constant_narrowing_conversion(
    env: &dyn TypeEnv,
    from: &Type,
    to: &Type,
    const_value: Option<ConstValue>,
) -> Option<Conversion> {
    let Some(ConstValue::Int(value)) = const_value else {
        return None;
    };

    let from = canonicalize_named(env, from);
    let to = canonicalize_named(env, to);

    let (Type::Primitive(from_p), Type::Primitive(to_p)) = (&from, &to) else {
        return None;
    };

    // JLS 5.2: allow narrowing for constant expressions of type byte/short/char/int
    // to byte/short/char when the value is representable.
    use PrimitiveType::*;
    if !matches!(*from_p, Byte | Short | Char | Int) {
        return None;
    }
    if !matches!(*to_p, Byte | Short | Char) {
        return None;
    }
    if !value_representable_in_primitive(value, *to_p) {
        return None;
    }

    Some(Conversion::new(ConversionStep::NarrowingPrimitive))
}

fn value_representable_in_primitive(value: i64, ty: PrimitiveType) -> bool {
    use PrimitiveType::*;
    match ty {
        Byte => (i64::from(i8::MIN)..=i64::from(i8::MAX)).contains(&value),
        Short => (i64::from(i16::MIN)..=i64::from(i16::MAX)).contains(&value),
        Char => (0..=i64::from(u16::MAX)).contains(&value),
        Int => (i64::from(i32::MIN)..=i64::from(i32::MAX)).contains(&value),
        Long => true,
        Float | Double | Boolean => false,
    }
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
                    Conversion::new(ConversionStep::Unboxing)
                        .push_step(ConversionStep::NarrowingPrimitive),
                );
            }
        }
    }

    // Reference casts.
    if from.is_reference() && to.is_reference() {
        match reference_castability(env, &from, &to) {
            Castability::No => {}
            castability => {
                let mut conv = Conversion::new(ConversionStep::NarrowingReference);
                if raw_warning(env, &from, &to) {
                    conv.warnings
                        .push(TypeWarning::Unchecked(UncheckedReason::RawConversion));
                } else if castability == Castability::Uncertain || !is_reifiable(env, &to) {
                    conv.warnings
                        .push(TypeWarning::Unchecked(UncheckedReason::UncheckedCast));
                }
                return Some(conv);
            }
        }
    }

    // Intersection casts: `(A & B) expr` is valid iff `expr` is castable to each component.
    if let Type::Intersection(parts) = &to {
        let mut conv = Conversion::new(ConversionStep::NarrowingReference);
        for p in parts {
            let part_conv = cast_conversion(env, &from, p)?;
            for warning in part_conv.warnings {
                if !conv.warnings.contains(&warning) {
                    conv.warnings.push(warning);
                }
            }
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
        Type::Class(ClassType { def, .. }) => {
            env.class(*def).and_then(|c| unbox_class_name(&c.name))
        }
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
    args.is_empty() && env.class(def).is_some_and(|c| !c.type_params.is_empty())
}

fn raw_warning(env: &dyn TypeEnv, from: &Type, to: &Type) -> bool {
    let (
        Type::Class(ClassType {
            def: f_def,
            args: f_args,
        }),
        Type::Class(ClassType {
            def: t_def,
            args: t_args,
        }),
    ) = (from, to)
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
    let (
        Type::Class(ClassType {
            def: f_def,
            args: f_args,
        }),
        Type::Class(ClassType {
            def: t_def,
            args: t_args,
        }),
    ) = (from, to)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Castability {
    Yes,
    No,
    Uncertain,
}

fn reference_castability(env: &dyn TypeEnv, from: &Type, to: &Type) -> Castability {
    if matches!(from, Type::Null) {
        return if to.is_reference() {
            Castability::Yes
        } else {
            Castability::No
        };
    }
    if is_subtype(env, from, to) || is_subtype(env, to, from) {
        return Castability::Yes;
    }

    match (from, to) {
        // Arrays: `S[]` is castable to `T[]` if the element types are castable.
        (Type::Array(from_elem), Type::Array(to_elem)) => match (&**from_elem, &**to_elem) {
            (Type::Primitive(a), Type::Primitive(b)) => {
                if a == b {
                    Castability::Yes
                } else {
                    Castability::No
                }
            }
            (a, b) if a.is_reference() && b.is_reference() => reference_castability(env, a, b),
            // Mixed primitive/reference arrays are never castable.
            _ => Castability::No,
        },

        // If one side is an array and we didn't hit a subtype relationship above,
        // the cast is invalid (arrays only implement Object/Cloneable/Serializable).
        (Type::Array(_), _) | (_, Type::Array(_)) => Castability::No,

        // Classes / interfaces.
        (
            Type::Class(ClassType { def: from_def, .. }),
            Type::Class(ClassType { def: to_def, .. }),
        ) => {
            let Some(from_kind) = env.class(*from_def).map(|c| c.kind) else {
                return Castability::Uncertain;
            };
            let Some(to_kind) = env.class(*to_def).map(|c| c.kind) else {
                return Castability::Uncertain;
            };

            match (from_kind, to_kind) {
                (ClassKind::Class, ClassKind::Class) => Castability::No,
                (ClassKind::Interface, _) | (_, ClassKind::Interface) => Castability::Yes,
            }
        }

        // Type variables / intersections: allow, but it's often unchecked.
        (Type::TypeVar(_), _) | (_, Type::TypeVar(_)) => Castability::Uncertain,
        (Type::Intersection(_), _) | (_, Type::Intersection(_)) => Castability::Uncertain,

        // Best-effort recovery: unknown / named / synthetic types are treated as castable.
        (Type::Named(_), _) | (_, Type::Named(_)) => Castability::Uncertain,
        (Type::VirtualInner { .. }, _) | (_, Type::VirtualInner { .. }) => Castability::Uncertain,
        (Type::Unknown | Type::Error, _) | (_, Type::Unknown | Type::Error) => Castability::Yes,

        _ => Castability::No,
    }
}

fn is_reifiable(_env: &dyn TypeEnv, ty: &Type) -> bool {
    match ty {
        Type::Primitive(_) => true,
        Type::Array(elem) => is_reifiable(_env, elem),
        Type::Class(ClassType { def: _, args }) => {
            if args.is_empty() {
                return true;
            }
            args.iter()
                .all(|a| matches!(a, Type::Wildcard(WildcardBound::Unbounded)))
        }
        Type::Named(_) | Type::VirtualInner { .. } => true,
        _ => false,
    }
}

/// Categorize a conversion for tie-breaking.
///
/// This is intended for overload resolution and diagnostic ranking:
/// `identity < widening < boxing/unboxing < unchecked < narrowing`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConversionCost {
    Identity,
    Widening,
    Boxing,
    Unchecked,
    Narrowing,
}

pub fn conversion_cost(conv: &Conversion) -> ConversionCost {
    let mut cost = ConversionCost::Identity;
    for step in &conv.steps {
        let step_cost = match step {
            ConversionStep::Identity => ConversionCost::Identity,
            ConversionStep::WideningPrimitive | ConversionStep::WideningReference => {
                ConversionCost::Widening
            }
            ConversionStep::Boxing | ConversionStep::Unboxing => ConversionCost::Boxing,
            ConversionStep::Unchecked => ConversionCost::Unchecked,
            ConversionStep::NarrowingPrimitive | ConversionStep::NarrowingReference => {
                ConversionCost::Narrowing
            }
        };
        cost = cost.max(step_cost);
    }
    if conv
        .warnings
        .iter()
        .any(|w| matches!(w, TypeWarning::Unchecked(_)))
    {
        cost = cost.max(ConversionCost::Unchecked);
    }
    cost
}

fn wildcard_upper_bound(env: &dyn TypeEnv, bound: &WildcardBound) -> Type {
    match bound {
        WildcardBound::Unbounded => Type::class(env.well_known().object, vec![]),
        WildcardBound::Extends(upper) => (**upper).clone(),
        // Wildcards with a lower bound (`? super T`) have `Object` as their upper bound.
        WildcardBound::Super(_) => Type::class(env.well_known().object, vec![]),
    }
}

fn type_arg_upper_bound_for_lub(env: &dyn TypeEnv, ty: &Type) -> Type {
    match ty {
        Type::Wildcard(bound) => wildcard_upper_bound(env, bound),
        other => other.clone(),
    }
}

fn canonicalize_for_lub(env: &dyn TypeEnv, ty: &Type) -> Type {
    match ty {
        Type::Named(name) => env
            .lookup_class(name)
            .map(|id| Type::class(id, vec![]))
            .unwrap_or_else(|| ty.clone()),
        Type::Wildcard(bound) => wildcard_upper_bound(env, bound),
        other => other.clone(),
    }
}

fn is_object_class(env: &dyn TypeEnv, ty: &Type) -> bool {
    matches!(
        ty,
        Type::Class(ClassType { def, args }) if *def == env.well_known().object && args.is_empty()
    )
}

fn type_sort_key(env: &dyn TypeEnv, ty: &Type) -> String {
    match ty {
        Type::Void => "void".to_string(),
        Type::Null => "null".to_string(),
        Type::Unknown => "<unknown>".to_string(),
        Type::Error => "<error>".to_string(),
        Type::Primitive(p) => format!("{p:?}"),
        Type::TypeVar(id) => format!("T{}", id.0),
        Type::Named(name) => format!("named:{name}"),
        Type::VirtualInner { owner, name } => format!("virtual:{}:{name}", owner.to_raw()),
        Type::Array(elem) => format!("{}[]", type_sort_key(env, elem)),
        Type::Wildcard(WildcardBound::Unbounded) => "?".to_string(),
        Type::Wildcard(WildcardBound::Extends(upper)) => {
            format!("? extends {}", type_sort_key(env, upper))
        }
        Type::Wildcard(WildcardBound::Super(lower)) => {
            format!("? super {}", type_sort_key(env, lower))
        }
        Type::Class(ClassType { def, args }) => {
            let mut out = env
                .class(*def)
                .map(|c| c.name.clone())
                .unwrap_or_else(|| format!("<class:{}>", def.to_raw()));
            if !args.is_empty() {
                out.push('<');
                for (idx, arg) in args.iter().enumerate() {
                    if idx > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(&type_sort_key(env, arg));
                }
                out.push('>');
            }
            out
        }
        Type::Intersection(types) => types
            .iter()
            .map(|t| type_sort_key(env, t))
            .collect::<Vec<_>>()
            .join(" & "),
    }
}

fn make_intersection(env: &dyn TypeEnv, types: Vec<Type>) -> Type {
    // Flatten all nested intersection components.
    let mut flat = Vec::new();
    let mut stack = types;
    while let Some(t) = stack.pop() {
        match t {
            Type::Intersection(parts) => stack.extend(parts),
            other => flat.push(other),
        }
    }

    let mut seen = HashSet::new();
    let mut uniq = Vec::new();
    for t in flat {
        if seen.insert(t.clone()) {
            uniq.push(t);
        }
    }

    if uniq.is_empty() {
        return Type::Intersection(Vec::new());
    }

    uniq.sort_by_cached_key(|ty| {
        // Canonical ordering for intersection components.
        //
        // Even though intersection types are commutative, Java's erasure rules use the *first*
        // bound for some computations (notably type variables and some intersection uses),
        // and Java syntax requires a class bound (if present) to appear first.
        //
        // Additionally, we want error recovery types (`Unknown`/`Error`) to dominate so that
        // we don't accidentally "hide" missing information by returning a concrete bound.
        let rank: u8 = match ty {
            Type::Unknown | Type::Error => 0,
            Type::Class(ClassType { def, .. }) => match env.class(*def).map(|c| c.kind) {
                Some(ClassKind::Interface) => 2,
                Some(ClassKind::Class) | None => 1,
            },
            Type::Named(name) => env
                .lookup_class(name)
                .and_then(|id| env.class(id))
                .map(|c| c.kind)
                .map(|k| match k {
                    ClassKind::Interface => 2,
                    ClassKind::Class => 1,
                })
                .unwrap_or(1),
            Type::Array(_) | Type::VirtualInner { .. } => 1,
            _ => 2,
        };
        (rank, type_sort_key(env, ty))
    });

    // Prune redundant supertypes (e.g. `ArrayList & List` => `ArrayList`), while
    // remaining deterministic in the face of our best-effort subtyping relation
    // (e.g. `Named` vs `Class`, and error recovery types like `Unknown`).
    //
    // Since `uniq` is sorted by `type_sort_key`, we always keep the first
    // representative when two types are mutually subtypes.
    let mut pruned: Vec<Type> = Vec::with_capacity(uniq.len());
    'cand: for t in uniq {
        // If we've already kept something at least as specific as `t`, drop `t`.
        for kept in &pruned {
            if is_subtype(env, kept, &t) {
                continue 'cand;
            }
        }

        // Otherwise `t` is not implied by anything we've kept, so remove anything
        // it implies (strict supertypes or mutually-subtype equivalents).
        pruned.retain(|kept| !is_subtype(env, &t, kept));
        pruned.push(t);
    }

    if pruned.len() == 1 {
        return pruned.into_iter().next().unwrap();
    }
    Type::Intersection(pruned)
}

fn lub_same_generic_class(
    env: &dyn TypeEnv,
    def: ClassId,
    a_args: &[Type],
    b_args: &[Type],
) -> Type {
    // Raw types behave like erasure: any instantiation is a subtype of the raw form,
    // and the raw form is the most useful LUB for IDE recovery.
    if is_raw_class(env, def, a_args) || is_raw_class(env, def, b_args) {
        return Type::class(def, vec![]);
    }

    if a_args.len() != b_args.len() {
        return Type::class(def, vec![]);
    }

    let mut out_args = Vec::with_capacity(a_args.len());
    for (a, b) in a_args.iter().zip(b_args) {
        if a == b {
            out_args.push(a.clone());
            continue;
        }

        let a_bound = type_arg_upper_bound_for_lub(env, a);
        let b_bound = type_arg_upper_bound_for_lub(env, b);
        let bound_lub = lub(env, &a_bound, &b_bound);
        if is_object_class(env, &bound_lub) {
            out_args.push(Type::Wildcard(WildcardBound::Unbounded));
        } else {
            out_args.push(Type::Wildcard(WildcardBound::Extends(Box::new(bound_lub))));
        }
    }

    Type::class(def, out_args)
}

fn collect_class_supertypes(
    env: &dyn TypeEnv,
    start_def: ClassId,
    start_args: Vec<Type>,
) -> HashMap<ClassId, Type> {
    let mut bucket: HashMap<ClassId, Vec<Type>> = HashMap::new();
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

        bucket
            .entry(def)
            .or_default()
            .push(Type::class(def, args.clone()));

        let Some(class_def) = env.class(def) else {
            continue;
        };

        let raw = is_raw_class(env, def, &args);
        let subst = class_def
            .type_params
            .iter()
            .copied()
            .zip(args.into_iter())
            .collect::<HashMap<_, _>>();

        if let Some(sc) = &class_def.super_class {
            let next = substitute(sc, &subst);
            queue.push_back(if raw { erasure(env, &next) } else { next });
        }
        for iface in &class_def.interfaces {
            let next = substitute(iface, &subst);
            queue.push_back(if raw { erasure(env, &next) } else { next });
        }
    }

    // Reduce multiple instantiations of the same class/interface to a single "best" representative.
    // (This is rare in practice but can happen in incomplete/erroneous environments.)
    let mut out = HashMap::new();
    for (def, insts) in bucket {
        let mut rep = insts
            .first()
            .cloned()
            .unwrap_or_else(|| Type::class(def, vec![]));
        for t in insts.iter().skip(1) {
            let (
                Type::Class(ClassType {
                    def: a_def,
                    args: a_args,
                }),
                Type::Class(ClassType {
                    def: b_def,
                    args: b_args,
                }),
            ) = (&rep, t)
            else {
                continue;
            };
            if a_def == b_def {
                rep = lub_same_generic_class(env, *a_def, a_args, b_args);
            }
        }
        out.insert(def, rep);
    }
    out
}

fn collect_supertypes_for_lub(env: &dyn TypeEnv, ty: &Type) -> HashMap<ClassId, Type> {
    let object = Type::class(env.well_known().object, vec![]);

    match ty {
        Type::Class(ClassType { def, args }) => {
            let mut out = collect_class_supertypes(env, *def, args.clone());
            out.insert(env.well_known().object, object);
            out
        }
        Type::Array(_) => {
            let wk = env.well_known();
            HashMap::from([
                (wk.object, Type::class(wk.object, vec![])),
                (wk.cloneable, Type::class(wk.cloneable, vec![])),
                (wk.serializable, Type::class(wk.serializable, vec![])),
            ])
        }
        Type::TypeVar(id) => {
            let mut out = HashMap::new();
            if let Some(tp) = env.type_param(*id) {
                for ub in &tp.upper_bounds {
                    let ub = canonicalize_for_lub(env, ub);
                    out.extend(collect_supertypes_for_lub(env, &ub));
                }
            }
            out.insert(env.well_known().object, object);
            out
        }
        Type::Intersection(parts) => {
            let mut out = HashMap::new();
            for p in parts {
                let p = canonicalize_for_lub(env, p);
                out.extend(collect_supertypes_for_lub(env, &p));
            }
            out.insert(env.well_known().object, object);
            out
        }
        Type::Named(name) => env
            .lookup_class(name)
            .map(|id| collect_supertypes_for_lub(env, &Type::class(id, vec![])))
            .unwrap_or_else(|| HashMap::from([(env.well_known().object, object)])),
        Type::VirtualInner { .. } => HashMap::from([(env.well_known().object, object)]),
        // `null` is always handled by the `a <: b` / `b <: a` fast-path.
        _ => HashMap::new(),
    }
}

fn minimal_common_supertypes(env: &dyn TypeEnv, candidates: &[Type]) -> Vec<Type> {
    let mut out = Vec::new();
    'outer: for t in candidates {
        for other in candidates {
            if t == other {
                continue;
            }
            // `t` is not minimal if there's a more specific common supertype.
            if is_subtype(env, other, t) {
                continue 'outer;
            }
        }
        out.push(t.clone());
    }
    out
}

fn lub_via_supertypes(env: &dyn TypeEnv, a: &Type, b: &Type) -> Type {
    let object = Type::class(env.well_known().object, vec![]);
    let sups_a = collect_supertypes_for_lub(env, a);
    let sups_b = collect_supertypes_for_lub(env, b);

    let mut common_defs: Vec<ClassId> = sups_a
        .keys()
        .filter(|d| sups_b.contains_key(d))
        .copied()
        .collect();
    common_defs.sort_by_key(|id| id.to_raw());

    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    for def in common_defs {
        let Some(Type::Class(ClassType { args: a_args, .. })) = sups_a.get(&def) else {
            continue;
        };
        let Some(Type::Class(ClassType { args: b_args, .. })) = sups_b.get(&def) else {
            continue;
        };
        let cand = lub_same_generic_class(env, def, a_args, b_args);
        if seen.insert(cand.clone()) {
            candidates.push(cand);
        }
    }

    if candidates.is_empty() {
        return object;
    }

    let mut minimals = minimal_common_supertypes(env, &candidates);
    if minimals.is_empty() {
        return object;
    }
    if minimals.len() == 1 {
        return minimals.pop().unwrap();
    }

    minimals.sort_by_cached_key(|a| type_sort_key(env, a));
    make_intersection(env, minimals)
}

/// Best-effort least-upper-bound computation for Java reference types.
///
/// This is intentionally not a full JLS 4.10.4 implementation, but it aims to
/// produce useful results for IDE scenarios (generic inference, conditional
/// expressions, etc.).
pub fn lub(env: &dyn TypeEnv, a: &Type, b: &Type) -> Type {
    // Error recovery: don't try to build synthetic intersection/wildcard types on top of
    // already-unknown data.
    if a.is_errorish() {
        return a.clone();
    }
    if b.is_errorish() {
        return b.clone();
    }

    // Preserve exact equality (including unresolved `Named` types).
    if a == b {
        return match a {
            Type::Intersection(_) => make_intersection(env, vec![a.clone()]),
            _ => a.clone(),
        };
    }

    let a = canonicalize_for_lub(env, a);
    let b = canonicalize_for_lub(env, b);

    let a_sub_b = is_subtype(env, &a, &b);
    let b_sub_a = is_subtype(env, &b, &a);

    match (a_sub_b, b_sub_a) {
        (true, false) => {
            return match b {
                Type::Intersection(_) => make_intersection(env, vec![b]),
                _ => b,
            };
        }
        (false, true) => {
            return match a {
                Type::Intersection(_) => make_intersection(env, vec![a]),
                _ => a,
            };
        }
        // Equivalent types (e.g. intersection permutations) should yield a deterministic,
        // normalized result.
        (true, true) => {
            return match (&a, &b) {
                (Type::Intersection(_), _) | (_, Type::Intersection(_)) => {
                    make_intersection(env, vec![a, b])
                }
                _ => {
                    if type_sort_key(env, &a) <= type_sort_key(env, &b) {
                        a
                    } else {
                        b
                    }
                }
            };
        }
        (false, false) => {}
    }

    match (&a, &b) {
        (Type::Array(a_elem), Type::Array(b_elem)) => {
            if a_elem.is_reference() && b_elem.is_reference() {
                Type::Array(Box::new(lub(env, a_elem, b_elem)))
            } else {
                // Arrays of primitive types (or mixed primitive/reference) only share the
                // `Object`, `Cloneable`, and `Serializable` supertypes.
                let wk = env.well_known();
                let cloneable = Type::class(wk.cloneable, vec![]);
                let serializable = Type::class(wk.serializable, vec![]);
                make_intersection(env, vec![cloneable, serializable])
            }
        }
        (
            Type::Class(ClassType {
                def: a_def,
                args: a_args,
            }),
            Type::Class(ClassType {
                def: b_def,
                args: b_args,
            }),
        ) if a_def == b_def => lub_same_generic_class(env, *a_def, a_args, b_args),
        _ => lub_via_supertypes(env, &a, &b),
    }
}

fn glb(env: &dyn TypeEnv, a: &Type, b: &Type) -> Type {
    // Preserve exact equality (including unresolved `Named` types).
    if a == b {
        // Still normalize intersections so we maintain the invariant that synthesized results are
        // flattened/deduped/sorted.
        return match a {
            Type::Intersection(_) => make_intersection(env, vec![a.clone()]),
            _ => a.clone(),
        };
    }

    let a_sub_b = is_subtype(env, a, b);
    let b_sub_a = is_subtype(env, b, a);

    match (a_sub_b, b_sub_a) {
        // Standard fast paths.
        (true, false) => match a {
            Type::Intersection(_) => make_intersection(env, vec![a.clone()]),
            _ => a.clone(),
        },
        (false, true) => match b {
            Type::Intersection(_) => make_intersection(env, vec![b.clone()]),
            _ => b.clone(),
        },

        // Error recovery (or other non-antisymmetric cases in our best-effort subtyping):
        // if the types are mutually compatible, pick a deterministic representative.
        //
        // Use `make_intersection` rather than a direct `type_sort_key` tie-breaker so we
        // fully normalize equivalent intersections (e.g. `(A & B & C)` in different orders).
        (true, true) => make_intersection(env, vec![a.clone(), b.clone()]),

        // Otherwise, synthesize a normalized intersection.
        (false, false) => make_intersection(env, vec![a.clone(), b.clone()]),
    }
}

// === Member resolution =======================================================

pub fn resolve_field(
    env: &dyn TypeEnv,
    receiver: &Type,
    name: &str,
    call_kind: CallKind,
) -> Option<FieldDef> {
    let mut receiver = receiver.clone();
    if let Type::Named(n) = &receiver {
        if let Some(id) = env.lookup_class(n) {
            receiver = Type::class(id, vec![]);
        }
    }

    let mut queue = VecDeque::new();
    let mut seen = HashSet::new();
    match receiver {
        Type::Intersection(types) => {
            for ty in types {
                match ty {
                    Type::Class(_) => queue.push_back(ty),
                    Type::Array(_) => queue.push_back(Type::class(env.well_known().object, vec![])),
                    Type::Named(n) => {
                        if let Some(id) = env.lookup_class(&n) {
                            queue.push_back(Type::class(id, vec![]));
                        } else {
                            queue.push_back(Type::Named(n));
                        }
                    }
                    _ => {}
                }
            }
        }
        Type::Class(_) => queue.push_back(receiver),
        Type::Array(_) => queue.push_back(Type::class(env.well_known().object, vec![])),
        _ => return None,
    }

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
            .zip(args.iter().cloned())
            .collect::<HashMap<_, _>>();

        for field in &class_def.fields {
            if field.name != name {
                continue;
            }

            let allowed = match (call_kind, field.is_static) {
                (CallKind::Static, true) => true,
                (CallKind::Instance, false) => true,
                // Best-effort: allow static fields from an instance receiver.
                (CallKind::Instance, true) => true,
                (CallKind::Static, false) => false,
            };
            if !allowed {
                continue;
            }

            return Some(FieldDef {
                name: field.name.clone(),
                ty: substitute(&field.ty, &subst),
                is_static: field.is_static,
                is_final: field.is_final,
            });
        }

        if let Some(sc) = &class_def.super_class {
            queue.push_back(substitute(sc, &subst));
        }
        for iface in &class_def.interfaces {
            queue.push_back(substitute(iface, &subst));
        }
        // In Java, every interface implicitly has `Object` as a supertype (JLS 4.10.2).
        if class_def.kind == ClassKind::Interface {
            queue.push_back(Type::class(env.well_known().object, vec![]));
        }
    }

    None
}

// === Method resolution =======================================================

#[derive(Debug, Clone)]
pub struct MethodCall<'a> {
    pub receiver: Type,
    pub call_kind: CallKind,
    pub name: &'a str,
    pub args: Vec<Type>,
    pub expected_return: Option<Type>,
    pub explicit_type_args: Vec<Type>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedMethod {
    pub owner: ClassId,
    pub name: String,
    /// Effective parameter types for the selected invocation (one per argument).
    ///
    /// For varargs methods invoked in variable-arity form, this list is expanded so it
    /// matches the call-site arity.
    pub params: Vec<Type>,
    /// Parameter types as they appear in the declared signature, when they differ from `params`.
    ///
    /// This is primarily used for variable-arity varargs invocations: `params` is expanded to match
    /// the call-site arity, but pretty-printers generally want to show the declared `T...` parameter.
    pub signature_params: Option<Vec<Type>>,
    pub return_type: Type,
    pub is_varargs: bool,
    pub is_static: bool,
    pub conversions: Vec<Conversion>,
    pub inferred_type_args: Vec<Type>,
    pub warnings: Vec<TypeWarning>,
    pub used_varargs: bool,
    pub phase: MethodSearchPhase,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodCandidate {
    pub owner: ClassId,
    pub name: String,
    pub params: Vec<Type>,
    pub return_type: Type,
    pub is_static: bool,
    pub is_varargs: bool,
    pub type_param_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodSearchPhase {
    Strict,
    Loose,
    Varargs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MethodCandidateFailureReason {
    WrongCallKind {
        call_kind: CallKind,
    },
    WrongArity {
        expected: usize,
        found: usize,
        is_varargs: bool,
    },
    ExplicitTypeArgCountMismatch {
        expected: usize,
        found: usize,
    },
    TypeArgOutOfBounds {
        type_param: TypeVarId,
        type_arg: Type,
        upper_bound: Type,
    },
    ArgumentConversion {
        arg_index: usize,
        from: Type,
        to: Type,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodCandidateFailure {
    pub phase: MethodSearchPhase,
    pub reason: MethodCandidateFailureReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodCandidateDiagnostics {
    pub candidate: MethodCandidate,
    pub failures: Vec<MethodCandidateFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodNotFound {
    pub receiver: Type,
    pub name: String,
    pub args: Vec<Type>,
    pub candidates: Vec<MethodCandidateDiagnostics>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodAmbiguity {
    pub phase: MethodSearchPhase,
    /// Applicable candidates for the selected phase, sorted from "best" to "worst".
    pub candidates: Vec<ResolvedMethod>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MethodResolution {
    Found(ResolvedMethod),
    NotFound(MethodNotFound),
    Ambiguous(MethodAmbiguity),
}

fn resolve_method_call_impl(
    env: &dyn TypeEnv,
    call: &MethodCall<'_>,
    receiver: Type,
) -> MethodResolution {
    let candidates = collect_method_candidates(env, &receiver, call.name);

    if candidates.is_empty() {
        return MethodResolution::NotFound(MethodNotFound {
            receiver,
            name: call.name.to_string(),
            args: call.args.clone(),
            candidates: Vec::new(),
        });
    }

    let mut diagnostics: Vec<MethodCandidateDiagnostics> = candidates
        .iter()
        .map(|cand| {
            let base_params = cand
                .method
                .params
                .iter()
                .map(|t| substitute(t, &cand.class_subst))
                .collect::<Vec<_>>();
            let base_return = substitute(&cand.method.return_type, &cand.class_subst);
            MethodCandidateDiagnostics {
                candidate: MethodCandidate {
                    owner: cand.owner,
                    name: cand.method.name.clone(),
                    params: base_params,
                    return_type: base_return,
                    is_static: cand.method.is_static,
                    is_varargs: cand.method.is_varargs,
                    type_param_count: cand.method.type_params.len(),
                },
                failures: Vec::new(),
            }
        })
        .collect();

    for phase in [
        MethodSearchPhase::Strict,
        MethodSearchPhase::Loose,
        MethodSearchPhase::Varargs,
    ] {
        let mut applicable: Vec<ResolvedMethod> = Vec::new();
        for (idx, cand) in candidates.iter().enumerate() {
            if call.call_kind == CallKind::Static && !cand.method.is_static {
                diagnostics[idx].failures.push(MethodCandidateFailure {
                    phase,
                    reason: MethodCandidateFailureReason::WrongCallKind {
                        call_kind: call.call_kind,
                    },
                });
                continue;
            }

            match check_applicability(env, cand, call, phase) {
                Ok(resolved) => applicable.push(resolved),
                Err(reason) => diagnostics[idx]
                    .failures
                    .push(MethodCandidateFailure { phase, reason }),
            }
        }

        if applicable.is_empty() {
            continue;
        }

        let mut ranked = applicable;
        rank_resolved_methods(env, call, &mut ranked);
        return match pick_best_method(env, call, &ranked, call.args.len()) {
            Some(best_idx) => MethodResolution::Found(ranked.swap_remove(best_idx)),
            None => MethodResolution::Ambiguous(MethodAmbiguity {
                phase,
                candidates: ranked,
            }),
        };
    }

    MethodResolution::NotFound(MethodNotFound {
        receiver,
        name: call.name.to_string(),
        args: call.args.clone(),
        candidates: diagnostics,
    })
}

pub fn resolve_constructor_call(
    env: &dyn TypeEnv,
    class: ClassId,
    args: &[Type],
    expected: Option<&Type>,
) -> MethodResolution {
    let receiver = match expected {
        Some(Type::Class(ClassType { def, args })) if *def == class => {
            Type::class(class, args.clone())
        }
        _ => Type::class(class, vec![]),
    };
    let receiver_args = match &receiver {
        Type::Class(ClassType { args, .. }) => args.clone(),
        _ => Vec::new(),
    };
    let return_type = receiver.clone();

    let call = MethodCall {
        receiver,
        call_kind: CallKind::Instance,
        name: "<init>",
        args: args.to_vec(),
        expected_return: expected.cloned(),
        explicit_type_args: vec![],
    };

    let Some(class_def) = env.class(class) else {
        return MethodResolution::NotFound(MethodNotFound {
            receiver: call.receiver.clone(),
            name: call.name.to_string(),
            args: call.args.clone(),
            candidates: Vec::new(),
        });
    };

    let class_subst = class_def
        .type_params
        .iter()
        .copied()
        .zip(receiver_args)
        .collect::<HashMap<_, _>>();

    let candidates: Vec<CandidateMethod> = class_def
        .constructors
        .iter()
        .filter(|c| c.is_accessible)
        .map(|ctor| CandidateMethod {
            owner: class,
            method: MethodDef {
                name: "<init>".to_string(),
                type_params: vec![],
                params: ctor.params.clone(),
                return_type: return_type.clone(),
                is_static: false,
                is_varargs: ctor.is_varargs,
                is_abstract: false,
            },
            class_subst: class_subst.clone(),
        })
        .collect();

    if candidates.is_empty() {
        return MethodResolution::NotFound(MethodNotFound {
            receiver: call.receiver.clone(),
            name: call.name.to_string(),
            args: call.args.clone(),
            candidates: Vec::new(),
        });
    }

    let mut diagnostics: Vec<MethodCandidateDiagnostics> = candidates
        .iter()
        .map(|cand| {
            let base_params = cand
                .method
                .params
                .iter()
                .map(|t| substitute(t, &cand.class_subst))
                .collect::<Vec<_>>();
            let base_return = substitute(&cand.method.return_type, &cand.class_subst);
            MethodCandidateDiagnostics {
                candidate: MethodCandidate {
                    owner: cand.owner,
                    name: cand.method.name.clone(),
                    params: base_params,
                    return_type: base_return,
                    is_static: cand.method.is_static,
                    is_varargs: cand.method.is_varargs,
                    type_param_count: cand.method.type_params.len(),
                },
                failures: Vec::new(),
            }
        })
        .collect();

    for phase in [
        MethodSearchPhase::Strict,
        MethodSearchPhase::Loose,
        MethodSearchPhase::Varargs,
    ] {
        let mut applicable: Vec<ResolvedMethod> = Vec::new();

        for (idx, cand) in candidates.iter().enumerate() {
            match check_applicability(env, cand, &call, phase) {
                Ok(resolved) => applicable.push(resolved),
                Err(reason) => diagnostics[idx]
                    .failures
                    .push(MethodCandidateFailure { phase, reason }),
            }
        }

        if applicable.is_empty() {
            continue;
        }

        let mut ranked = applicable;
        rank_resolved_methods(env, &call, &mut ranked);
        return match pick_best_method(env, &call, &ranked, call.args.len()) {
            Some(best_idx) => MethodResolution::Found(ranked.swap_remove(best_idx)),
            None => MethodResolution::Ambiguous(MethodAmbiguity {
                phase,
                candidates: ranked,
            }),
        };
    }

    MethodResolution::NotFound(MethodNotFound {
        receiver: call.receiver.clone(),
        name: call.name.to_string(),
        args: call.args.clone(),
        candidates: diagnostics,
    })
}

#[derive(Debug, Clone)]
struct CandidateMethod {
    owner: ClassId,
    method: MethodDef,
    class_subst: HashMap<TypeVarId, Type>,
}

fn collect_method_candidates(
    env: &dyn TypeEnv,
    receiver: &Type,
    name: &str,
) -> Vec<CandidateMethod> {
    let mut out = Vec::new();
    // Track candidates we've already seen by erased signature so we don't return duplicates
    // from overridden/hiding methods. For intersection types, we may encounter the same method
    // signature across multiple bounds; in those cases we merge return types to preserve the most
    // specific/precise result (`Integer` vs `Number`, or an `A & B` intersection).
    let mut seen_sigs: HashMap<(bool, Vec<Type>), usize> = HashMap::new();

    let mut queue = VecDeque::new();
    let mut seen = HashSet::new();
    fn push_receiver_for_lookup(env: &dyn TypeEnv, queue: &mut VecDeque<Type>, ty: &Type) {
        match ty {
            Type::Intersection(types) => {
                for t in types {
                    push_receiver_for_lookup(env, queue, t);
                }
            }
            Type::Class(_) => queue.push_back(ty.clone()),
            Type::Array(_) => queue.push_back(Type::class(env.well_known().object, vec![])),
            Type::Named(n) => {
                if let Some(id) = env.lookup_class(n) {
                    queue.push_back(Type::class(id, vec![]));
                }
            }
            _ => {}
        }
    }
    push_receiver_for_lookup(env, &mut queue, receiver);
    if queue.is_empty() {
        return out;
    }

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
                // Best-effort override/hiding handling:
                // if we've already seen a method with the same erased signature, keep the
                // most specific declaration (we traverse from receiver -> supertypes).
                let erased_params = method
                    .params
                    .iter()
                    .map(|t| erasure(env, &substitute(t, &subst)))
                    .collect::<Vec<_>>();
                let sig_key = (method.is_static, erased_params);

                if let Some(&existing) = seen_sigs.get(&sig_key) {
                    // Best-effort intersection handling: if two unrelated bounds declare the same
                    // generic method, their type parameter ids will differ even though the methods
                    // are "the same" up to alpha-renaming. To avoid leaking an unrelated type var
                    // into the merged return type (`String & V`), rewrite the current method's
                    // type vars to the existing candidate's ids (by position) before computing the
                    // GLB return type.
                    if method.type_params.len() != out[existing].method.type_params.len() {
                        continue;
                    }
                    let existing_return = substitute(
                        &out[existing].method.return_type,
                        &out[existing].class_subst,
                    );
                    let mut current_return = substitute(&method.return_type, &subst);
                    if !method.type_params.is_empty() {
                        let mut tv_subst = HashMap::with_capacity(method.type_params.len());
                        for (from, to) in method
                            .type_params
                            .iter()
                            .copied()
                            .zip(out[existing].method.type_params.iter().copied())
                        {
                            tv_subst.insert(from, Type::TypeVar(to));
                        }
                        current_return = substitute(&current_return, &tv_subst);
                    }
                    out[existing].method.return_type = glb(env, &existing_return, &current_return);
                    continue;
                }
                seen_sigs.insert(sig_key, out.len());
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
        // In Java, every interface implicitly has `Object` as a supertype (JLS 4.10.2).
        if class_def.kind == ClassKind::Interface {
            queue.push_back(Type::class(env.well_known().object, vec![]));
        }
    }

    out
}

fn check_applicability(
    env: &dyn TypeEnv,
    cand: &CandidateMethod,
    call: &MethodCall<'_>,
    phase: MethodSearchPhase,
) -> Result<ResolvedMethod, MethodCandidateFailureReason> {
    let method = &cand.method;
    let arity = call.args.len();

    // Arity precheck.
    match (method.is_varargs, phase) {
        (false, _) if arity != method.params.len() => {
            return Err(MethodCandidateFailureReason::WrongArity {
                expected: method.params.len(),
                found: arity,
                is_varargs: false,
            });
        }
        (true, MethodSearchPhase::Strict | MethodSearchPhase::Loose)
            if arity != method.params.len() =>
        {
            return Err(MethodCandidateFailureReason::WrongArity {
                expected: method.params.len(),
                found: arity,
                is_varargs: true,
            });
        }
        // Variable-arity invocation needs at least `fixed` arguments.
        (true, MethodSearchPhase::Varargs) if arity + 1 < method.params.len() => {
            return Err(MethodCandidateFailureReason::WrongArity {
                expected: method.params.len().saturating_sub(1),
                found: arity,
                is_varargs: true,
            });
        }
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
    if !(method.is_varargs && phase == MethodSearchPhase::Varargs && arity != base_params.len()) {
        if let Ok(res) = try_method_invocation(
            env,
            cand.owner,
            method,
            &base_params,
            &base_return_type,
            call,
            phase,
            false,
        ) {
            return Ok(res);
        }
    }

    // Varargs phase can also use variable-arity invocation.
    if method.is_varargs && phase == MethodSearchPhase::Varargs {
        return try_method_invocation(
            env,
            cand.owner,
            method,
            &base_params,
            &base_return_type,
            call,
            phase,
            true,
        );
    }

    try_method_invocation(
        env,
        cand.owner,
        method,
        &base_params,
        &base_return_type,
        call,
        phase,
        false,
    )
}

#[allow(clippy::too_many_arguments)]
fn try_method_invocation(
    env: &dyn TypeEnv,
    owner: ClassId,
    method: &MethodDef,
    base_params: &[Type],
    base_return_type: &Type,
    call: &MethodCall<'_>,
    phase: MethodSearchPhase,
    force_varargs: bool,
) -> Result<ResolvedMethod, MethodCandidateFailureReason> {
    let arity = call.args.len();

    let (pattern_params, used_varargs) =
        if method.is_varargs && (phase == MethodSearchPhase::Varargs) {
            if force_varargs {
                (
                    expand_varargs_pattern(base_params, arity).ok_or(
                        MethodCandidateFailureReason::WrongArity {
                            expected: base_params.len().saturating_sub(1),
                            found: arity,
                            is_varargs: true,
                        },
                    )?,
                    true,
                )
            } else {
                // Fixed-arity invocation (last param is the array type).
                if arity != base_params.len() {
                    return Err(MethodCandidateFailureReason::WrongArity {
                        expected: base_params.len(),
                        found: arity,
                        is_varargs: true,
                    });
                }
                (base_params.to_vec(), false)
            }
        } else if method.is_varargs {
            // Strict/loose: only fixed-arity invocation allowed.
            if arity != base_params.len() {
                return Err(MethodCandidateFailureReason::WrongArity {
                    expected: base_params.len(),
                    found: arity,
                    is_varargs: true,
                });
            }
            (base_params.to_vec(), false)
        } else {
            (base_params.to_vec(), false)
        };

    // Infer (or apply explicit) method type arguments using the effective parameter pattern.
    let inferred_type_args = if method.type_params.is_empty() {
        Vec::new()
    } else if !call.explicit_type_args.is_empty() {
        if call.explicit_type_args.len() != method.type_params.len() {
            return Err(MethodCandidateFailureReason::ExplicitTypeArgCountMismatch {
                expected: method.type_params.len(),
                found: call.explicit_type_args.len(),
            });
        }
        call.explicit_type_args.clone()
    } else {
        infer_type_arguments_from_call(env, method, &pattern_params, base_return_type, call)
    };

    let method_subst: HashMap<TypeVarId, Type> = method
        .type_params
        .iter()
        .copied()
        .zip(inferred_type_args.iter().cloned())
        .collect();

    let signature_params = if method.is_varargs && used_varargs {
        Some(
            base_params
                .iter()
                .map(|t| substitute(t, &method_subst))
                .collect(),
        )
    } else {
        None
    };

    // Validate inferred/explicit type arguments against the declared bounds.
    let object = Type::class(env.well_known().object, vec![]);
    for (tv, ty_arg) in method
        .type_params
        .iter()
        .copied()
        .zip(inferred_type_args.iter())
    {
        let upper_bounds = env
            .type_param(tv)
            .and_then(|tp| {
                if tp.upper_bounds.is_empty() {
                    None
                } else {
                    Some(tp.upper_bounds.clone())
                }
            })
            .unwrap_or_else(|| vec![object.clone()]);

        for ub in upper_bounds {
            let ub = substitute(&ub, &method_subst);
            if !is_subtype(env, ty_arg, &ub) {
                return Err(MethodCandidateFailureReason::TypeArgOutOfBounds {
                    type_param: tv,
                    type_arg: ty_arg.clone(),
                    upper_bound: ub,
                });
            }
        }
    }

    let effective_params: Vec<Type> = pattern_params
        .iter()
        .map(|t| substitute(t, &method_subst))
        .collect();
    if effective_params.len() != arity {
        return Err(MethodCandidateFailureReason::WrongArity {
            expected: effective_params.len(),
            found: arity,
            is_varargs: method.is_varargs,
        });
    }
    let return_type = substitute(base_return_type, &method_subst);

    let mut warnings = Vec::new();
    let mut conversions = Vec::with_capacity(arity);
    for (arg, param) in call.args.iter().zip(&effective_params) {
        let conv = match phase {
            MethodSearchPhase::Strict => strict_method_invocation_conversion(env, arg, param),
            MethodSearchPhase::Loose | MethodSearchPhase::Varargs => {
                method_invocation_conversion(env, arg, param)
            }
        }
        .ok_or_else(|| MethodCandidateFailureReason::ArgumentConversion {
            arg_index: conversions.len(),
            from: arg.clone(),
            to: param.clone(),
        })?;
        warnings.extend(conv.warnings.iter().cloned());
        conversions.push(conv);
    }

    // Best-effort unchecked varargs warning: when a variable-arity invocation triggers
    // array creation for a non-reifiable varargs parameter type, surface `-Xlint:unchecked`
    // style diagnostics (JLS 15.12.2.4).
    if used_varargs {
        if let Some(varargs_param) = base_params.last() {
            if !is_reifiable(env, varargs_param) {
                warnings.push(TypeWarning::Unchecked(UncheckedReason::UncheckedVarargs));
            }
        }
    }

    if call.call_kind == CallKind::Instance && method.is_static {
        warnings.push(TypeWarning::StaticAccessViaInstance);
    }

    Ok(ResolvedMethod {
        owner,
        name: method.name.clone(),
        params: effective_params,
        signature_params,
        return_type,
        is_varargs: method.is_varargs,
        is_static: method.is_static,
        conversions,
        inferred_type_args,
        warnings,
        used_varargs,
        phase,
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
    if tys.is_empty() {
        return object.clone();
    }

    // Sort first for determinism even when our subtyping relation is best-effort
    // (e.g. error recovery types like Unknown/Error, or Named vs resolved Class types).
    let mut sorted = tys.to_vec();
    sorted.sort_by_cached_key(|t| type_sort_key(env, t));

    let mut it = sorted.into_iter();
    let first = it.next().unwrap_or_else(|| object.clone());
    // Ensure any pre-existing intersection is normalized even when there is only
    // a single bound (so we never leak a non-canonical `Type::Intersection`).
    let mut acc = make_intersection(env, vec![first]);
    for t in it {
        acc = glb(env, &acc, &t);
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
        acc = lub(env, &acc, t);
    }
    acc
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
        Type::Class(ClassType {
            def: p_def,
            args: p_args,
        }) => {
            if let Type::Class(ClassType {
                def: a_def,
                args: a_args,
            }) = arg
            {
                // Fast path: same generic class/interface.
                if p_def == a_def && p_args.len() == a_args.len() {
                    for (a, p) in a_args.iter().zip(p_args) {
                        collect_type_arg_constraints(env, a, p, bounds);
                    }
                    return;
                }

                // Common Java inference case: the argument is a subtype whose generic
                // supertype matches the formal parameter. Example:
                //   formal:  List<T>
                //   actual:  ArrayList<String>
                // We map `actual` to an instantiation of `formal`'s generic class via
                // supertypes and collect constraints against that view.
                if p_def != a_def {
                    // Be conservative around raw types and malformed instantiations: if we
                    // don't have enough info to map type arguments correctly, skip.
                    let (Some(actual_def), Some(formal_def)) =
                        (env.class(*a_def), env.class(*p_def))
                    else {
                        return;
                    };
                    if actual_def.type_params.len() != a_args.len()
                        || formal_def.type_params.len() != p_args.len()
                    {
                        return;
                    }
                    if is_raw_class(env, *a_def, a_args) || is_raw_class(env, *p_def, p_args) {
                        return;
                    }

                    let Some(mapped_args) = instantiate_as(env, *a_def, a_args.clone(), *p_def)
                    else {
                        return;
                    };
                    if mapped_args.len() != p_args.len() {
                        return;
                    }
                    for (a, p) in mapped_args.iter().zip(p_args) {
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
    _env: &dyn TypeEnv,
    lower: &Type,
    actual: &Type,
    bounds: &mut HashMap<TypeVarId, InferenceBounds>,
) {
    // lower <: actual
    match lower {
        Type::TypeVar(tv) => push_upper_bound(bounds, *tv, actual.clone()),
        Type::Class(ClassType {
            def: l_def,
            args: l_args,
        }) => {
            if let Type::Class(ClassType {
                def: a_def,
                args: a_args,
            }) = actual
            {
                if l_def == a_def && l_args.len() == a_args.len() {
                    for (l, a) in l_args.iter().zip(a_args) {
                        collect_reverse_constraints(_env, l, a, bounds);
                    }
                }
            }
        }
        _ => {}
    }
}

fn collect_equality_constraints(
    _env: &dyn TypeEnv,
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
                collect_equality_constraints(_env, a_elem, f_elem, bounds);
            }
        }
        Type::Class(ClassType {
            def: f_def,
            args: f_args,
        }) => {
            if let Type::Class(ClassType {
                def: a_def,
                args: a_args,
            }) = actual
            {
                if f_def == a_def && f_args.len() == a_args.len() {
                    for (a, f) in a_args.iter().zip(f_args) {
                        collect_equality_constraints(_env, a, f, bounds);
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
        Type::Class(ClassType {
            def: r_def,
            args: r_args,
        }) => {
            if let Type::Class(ClassType {
                def: e_def,
                args: e_args,
            }) = expected
            {
                // Fast path: expected is the same generic class/interface.
                if r_def == e_def && r_args.len() == e_args.len() {
                    for (r, e) in r_args.iter().zip(e_args) {
                        collect_equality_constraints(env, e, r, bounds);
                    }
                    return;
                }

                // Return context can constrain method type variables through a supertype
                // relationship, e.g.:
                //   ret:      ArrayList<T>
                //   expected: List<String>
                // We map `ret` to an instantiation of `expected`'s generic class via
                // supertypes and collect constraints against that view.
                if r_def != e_def {
                    // Be conservative around raw types and malformed instantiations.
                    let (Some(ret_def), Some(expected_def)) =
                        (env.class(*r_def), env.class(*e_def))
                    else {
                        return;
                    };
                    if ret_def.type_params.len() != r_args.len()
                        || expected_def.type_params.len() != e_args.len()
                    {
                        return;
                    }
                    if is_raw_class(env, *r_def, r_args) || is_raw_class(env, *e_def, e_args) {
                        return;
                    }

                    let Some(mapped_args) = instantiate_as(env, *r_def, r_args.clone(), *e_def)
                    else {
                        return;
                    };
                    if mapped_args.len() != e_args.len() {
                        return;
                    }
                    for (r, e) in mapped_args.iter().zip(e_args) {
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

fn collect_type_var_constraints(
    mapping: &mut HashMap<TypeVarId, Type>,
    pattern: &Type,
    actual: &Type,
) {
    match pattern {
        Type::TypeVar(id) => insert_type_var_constraint(mapping, *id, actual),
        Type::Array(p_elem) => {
            if let Type::Array(a_elem) = actual {
                collect_type_var_constraints(mapping, p_elem, a_elem);
            }
        }
        Type::Class(ClassType {
            def: p_def,
            args: p_args,
        }) => {
            if let Type::Class(ClassType {
                def: a_def,
                args: a_args,
            }) = actual
            {
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

fn insert_type_var_constraint(
    mapping: &mut HashMap<TypeVarId, Type>,
    id: TypeVarId,
    actual: &Type,
) {
    use std::collections::hash_map::Entry;

    match mapping.entry(id) {
        Entry::Vacant(v) => {
            v.insert(actual.clone());
        }
        Entry::Occupied(mut o) => {
            let current = o.get();
            if is_placeholder_type_for_inference(current)
                && !is_placeholder_type_for_inference(actual)
            {
                o.insert(actual.clone());
            }
        }
    }
}

fn is_placeholder_type_for_inference(ty: &Type) -> bool {
    matches!(ty, Type::Unknown | Type::Error | Type::Null)
}

fn conversion_score(conv: &Conversion) -> u32 {
    let tier = match conversion_cost(conv) {
        ConversionCost::Identity => 0,
        ConversionCost::Widening => 1,
        ConversionCost::Boxing => 2,
        ConversionCost::Unchecked => 3,
        ConversionCost::Narrowing => 4,
    };
    tier * 10 + conv.steps.len() as u32
}

fn total_conversion_score(method: &ResolvedMethod) -> u32 {
    method.conversions.iter().map(conversion_score).sum()
}

fn rank_resolved_methods(
    _env: &dyn TypeEnv,
    call: &MethodCall<'_>,
    methods: &mut [ResolvedMethod],
) {
    methods.sort_by(|a, b| {
        let ka = (
            u8::from(call.call_kind == CallKind::Instance && a.is_static),
            u8::from(a.is_varargs),
            u8::from(a.used_varargs),
            total_conversion_score(a),
            u8::from(!a.inferred_type_args.is_empty()),
            a.warnings.len(),
        );
        let kb = (
            u8::from(call.call_kind == CallKind::Instance && b.is_static),
            u8::from(b.is_varargs),
            u8::from(b.used_varargs),
            total_conversion_score(b),
            u8::from(!b.inferred_type_args.is_empty()),
            b.warnings.len(),
        );
        ka.cmp(&kb)
    });
}
fn is_more_specific(
    env: &dyn TypeEnv,
    a: &ResolvedMethod,
    b: &ResolvedMethod,
    arity: usize,
) -> bool {
    if a.used_varargs != b.used_varargs {
        return !a.used_varargs && b.used_varargs;
    }

    if a.is_varargs != b.is_varargs {
        return !a.is_varargs && b.is_varargs;
    }

    if a.params.len() != arity || b.params.len() != arity {
        return false;
    }

    a.params
        .iter()
        .zip(&b.params)
        .all(|(a_ty, b_ty)| is_subtype(env, a_ty, b_ty))
}

fn is_more_specific_instantiation(
    env: &dyn TypeEnv,
    a: &ResolvedMethod,
    b: &ResolvedMethod,
) -> bool {
    if a.inferred_type_args.is_empty()
        || b.inferred_type_args.is_empty()
        || a.inferred_type_args.len() != b.inferred_type_args.len()
    {
        return false;
    }

    let mut strictly = false;
    for (a_arg, b_arg) in a.inferred_type_args.iter().zip(&b.inferred_type_args) {
        if !is_subtype(env, a_arg, b_arg) {
            return false;
        }
        strictly |= a_arg != b_arg;
    }
    strictly
}

fn pick_best_method(
    env: &dyn TypeEnv,
    call: &MethodCall<'_>,
    methods: &[ResolvedMethod],
    arity: usize,
) -> Option<usize> {
    if methods.is_empty() {
        return None;
    }

    // First, keep methods that are not strictly less specific than another (JLS-inspired).
    let mut maximal: Vec<usize> = Vec::new();
    'outer: for (idx, m) in methods.iter().enumerate() {
        for (other_idx, other) in methods.iter().enumerate() {
            if idx == other_idx {
                continue;
            }
            if is_more_specific(env, other, m, arity) && !is_more_specific(env, m, other, arity) {
                continue 'outer;
            }
        }
        maximal.push(idx);
    }

    if maximal.len() == 1 {
        return Some(maximal[0]);
    }
    if maximal.is_empty() {
        return None;
    }

    let mut candidates = maximal;

    // Instance calls: prefer instance methods, but keep static ones for best-effort behavior.
    if call.call_kind == CallKind::Instance && candidates.iter().any(|&i| !methods[i].is_static) {
        candidates.retain(|&i| !methods[i].is_static);
        if candidates.len() == 1 {
            return Some(candidates[0]);
        }
    }

    // Prefer declarations in a more specific type when the invocation signature ties.
    // This is primarily for override/bridge duplicates that survive candidate collection.
    let mut filtered = Vec::new();
    'keep: for &i in &candidates {
        for &j in &candidates {
            if i == j {
                continue;
            }
            let mi = &methods[i];
            let mj = &methods[j];
            if mi.params == mj.params
                && mi.is_static == mj.is_static
                && mi.is_varargs == mj.is_varargs
            {
                let ti = Type::class(mi.owner, vec![]);
                let tj = Type::class(mj.owner, vec![]);
                if is_subtype(env, &tj, &ti) && !is_subtype(env, &ti, &tj) {
                    continue 'keep;
                }
            }
        }
        filtered.push(i);
    }
    candidates = filtered;
    if candidates.len() == 1 {
        return Some(candidates[0]);
    }

    // Prefer non-varargs methods over varargs methods.
    if candidates.iter().any(|&i| !methods[i].is_varargs) {
        candidates.retain(|&i| !methods[i].is_varargs);
        if candidates.len() == 1 {
            return Some(candidates[0]);
        }
    }

    // Prefer fixed-arity invocation over varargs expansion.
    if candidates.iter().any(|&i| !methods[i].used_varargs) {
        candidates.retain(|&i| !methods[i].used_varargs);
        if candidates.len() == 1 {
            return Some(candidates[0]);
        }
    }

    // Prefer cheaper conversions.
    let min_cost = candidates
        .iter()
        .map(|&i| total_conversion_score(&methods[i]))
        .min()
        .unwrap_or(u32::MAX);
    candidates.retain(|&i| total_conversion_score(&methods[i]) == min_cost);
    if candidates.len() == 1 {
        return Some(candidates[0]);
    }

    // Prefer more specific generic instantiations when comparing generic methods.
    if candidates
        .iter()
        .all(|&i| !methods[i].inferred_type_args.is_empty())
    {
        let mut inst_max: Vec<usize> = Vec::new();
        'inst: for &i in &candidates {
            for &j in &candidates {
                if i == j {
                    continue;
                }
                if is_more_specific_instantiation(env, &methods[j], &methods[i]) {
                    continue 'inst;
                }
            }
            inst_max.push(i);
        }
        if inst_max.len() == 1 {
            return Some(inst_max[0]);
        }
        if !inst_max.is_empty() && inst_max.len() < candidates.len() {
            candidates = inst_max;
        }
    }

    // Prefer non-generic methods when parameter types tie.
    if candidates
        .iter()
        .any(|&i| methods[i].inferred_type_args.is_empty())
    {
        candidates.retain(|&i| methods[i].inferred_type_args.is_empty());
        if candidates.len() == 1 {
            return Some(candidates[0]);
        }
    }

    // Prefer fewer warnings (unchecked/raw conversions, static access via instance).
    let min_warnings = candidates
        .iter()
        .map(|&i| methods[i].warnings.len())
        .min()
        .unwrap_or(usize::MAX);
    candidates.retain(|&i| methods[i].warnings.len() == min_warnings);
    if candidates.len() == 1 {
        return Some(candidates[0]);
    }

    None
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

pub fn infer_diamond_type_args(
    env: &dyn TypeEnv,
    class: ClassId,
    target: Option<&Type>,
) -> Vec<Type> {
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
            Type::Named(name) => env.lookup_class(name).map(|id| ClassType {
                def: id,
                args: vec![],
            }),
            _ => None,
        };

        if let Some(target_class) = target_class {
            if !target_class.args.is_empty() {
                if let Some(mapping) = infer_class_type_arguments_from_target(
                    env,
                    class,
                    target_class.def,
                    &target_class.args,
                ) {
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
    infer_lambda_sam_signature(env, target).map(|sig| sig.params)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LambdaSamSignature {
    pub params: Vec<Type>,
    pub return_type: Type,
}

pub fn infer_lambda_sam_signature(env: &dyn TypeEnv, target: &Type) -> Option<LambdaSamSignature> {
    let target = canonicalize_named(env, target);
    let Type::Class(ClassType { def, args }) = target else {
        return None;
    };

    let class_def = env.class(def)?;

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
    let params = sam.params.iter().map(|t| substitute(t, &subst)).collect();
    let return_type = substitute(&sam.return_type, &subst);
    Some(LambdaSamSignature {
        params,
        return_type,
    })
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
            .zip(owner_instantiation),
    );

    subst
}

/// Instantiate `ty` as `target_def`, returning the type arguments of `target_def`
/// as seen through `ty`'s inheritance chain.
///
/// This is a common operation for Java target typing and generic analysis:
/// given a receiver type `C<...>` and a desired supertype/interface `S`, compute
/// `S<...>` by substituting type parameters through `C`'s declared supertypes.
///
/// Returns `None` if the inheritance chain cannot be traversed, or `target_def` is not a supertype
/// of `ty`.
pub fn instantiate_supertype(
    env: &dyn TypeEnv,
    ty: &Type,
    target_def: ClassId,
) -> Option<Vec<Type>> {
    match ty {
        Type::Class(ClassType { def, args }) => instantiate_as(env, *def, args.clone(), target_def),
        Type::Named(name) => {
            let def = env.lookup_class(name)?;
            instantiate_as(env, def, vec![], target_def)
        }
        Type::TypeVar(id) => env
            .type_param(*id)
            .and_then(|tp| tp.upper_bounds.first())
            .and_then(|b| instantiate_supertype(env, b, target_def)),
        Type::Intersection(parts) => parts
            .iter()
            .find_map(|p| instantiate_supertype(env, p, target_def)),
        _ => None,
    }
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

pub fn type_of<'env>(ctx: &mut TyContext<'env>, expr: &Expr) -> Type {
    match expr {
        Expr::Null => Type::Null,
        Expr::Int(_) => Type::Primitive(PrimitiveType::Int),
        Expr::String(_) => Type::class(ctx.well_known().string, vec![]),
        Expr::MethodCall {
            receiver,
            name,
            args,
            expected_return,
        } => {
            let recv_ty = type_of(ctx, receiver);
            let arg_tys = args.iter().map(|a| type_of(ctx, a)).collect::<Vec<_>>();
            let call = MethodCall {
                receiver: recv_ty,
                call_kind: CallKind::Instance,
                name,
                args: arg_tys,
                expected_return: expected_return.clone(),
                explicit_type_args: vec![],
            };
            match resolve_method_call(ctx, &call) {
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
            fields: vec![],
            constructors: vec![],
            methods: vec![],
        });
        let dog = env.add_class(ClassDef {
            name: "Dog".to_string(),
            kind: ClassKind::Class,
            type_params: vec![],
            super_class: Some(Type::class(animal, vec![])),
            interfaces: vec![],
            fields: vec![],
            constructors: vec![],
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
            fields: vec![],
            constructors: vec![],
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
            call_kind: CallKind::Instance,
            name: "m",
            args: vec![Type::class(string, vec![])],
            expected_return: None,
            explicit_type_args: vec![],
        };

        let mut ctx = TyContext::new(&env);
        let MethodResolution::Found(found) = resolve_method_call(&mut ctx, &call) else {
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
    fn instantiate_supertype_arraylist_string_as_list() {
        let env = store();
        let array_list = env.class_id("java.util.ArrayList").unwrap();
        let list = env.class_id("java.util.List").unwrap();
        let string = Type::class(env.well_known().string, vec![]);

        let al_string = Type::class(array_list, vec![string.clone()]);
        let instantiated =
            instantiate_supertype(&env, &al_string, list).expect("should instantiate List<T>");
        assert_eq!(instantiated, vec![string]);
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
            fields: vec![],
            constructors: vec![],
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
            call_kind: CallKind::Static,
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
            fields: vec![],
            constructors: vec![],
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
            call_kind: CallKind::Static,
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

    #[test]
    fn lambda_sam_signature_inference_from_function_target() {
        let env = store();
        let function = env.class_id("java.util.function.Function").unwrap();
        let string = Type::class(env.well_known().string, vec![]);
        let integer = Type::class(env.well_known().integer, vec![]);

        let target = Type::class(function, vec![string.clone(), integer.clone()]);
        let sig =
            infer_lambda_sam_signature(&env, &target).expect("should infer lambda SAM signature");

        assert_eq!(sig.params, vec![string]);
        assert_eq!(sig.return_type, integer);
    }

    #[test]
    fn lambda_sam_signature_inference_from_runnable_target() {
        let env = store();
        let runnable = env.class_id("java.lang.Runnable").unwrap();
        let target = Type::class(runnable, vec![]);
        let sig =
            infer_lambda_sam_signature(&env, &target).expect("should infer lambda SAM signature");
        assert_eq!(sig.params, Vec::<Type>::new());
        assert_eq!(sig.return_type, Type::Void);
    }

    #[test]
    fn lambda_param_inference_from_consumer_target() {
        let env = store();
        let consumer = env.class_id("java.util.function.Consumer").unwrap();
        let string = Type::class(env.well_known().string, vec![]);
        let target = Type::class(consumer, vec![string.clone()]);
        let params = infer_lambda_param_types(&env, &target).expect("should infer lambda params");
        assert_eq!(params, vec![string]);
    }

    #[test]
    fn collections_empty_list_infers_type_from_expected_return() {
        let env = store();
        let collections = env.class_id("java.util.Collections").unwrap();
        let list = env.class_id("java.util.List").unwrap();
        let string = Type::class(env.well_known().string, vec![]);

        let expected_return = Type::class(list, vec![string.clone()]);
        let call = MethodCall {
            receiver: Type::class(collections, vec![]),
            call_kind: CallKind::Static,
            name: "emptyList",
            args: vec![],
            expected_return: Some(expected_return),
            explicit_type_args: vec![],
        };
        let method = &env.class(collections).unwrap().methods[0];
        let inferred = infer_type_arguments(&env, &call, collections, method);
        assert_eq!(inferred, vec![string]);
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
