//! Shared types used across Nova crates.
//!
//! The upstream Nova project has a much richer type system. In this kata repo we
//! keep this crate intentionally small and focused on what early framework
//! analyzers/tests need.

use std::fmt;

use serde::{Deserialize, Serialize};

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
pub struct ProjectId(u32);

impl ProjectId {
    pub fn new(raw: u32) -> Self {
        Self(raw)
    }
}

/// Identifier for a Java class (top-level or nested).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ClassId(u32);

impl ClassId {
    pub fn new(raw: u32) -> Self {
        Self(raw)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PrimitiveType {
    Boolean,
    Int,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Type {
    Void,
    Primitive(PrimitiveType),
    /// Refers to a class known to the framework database.
    Class(ClassId),
    /// Refers to a class not tracked by the database (e.g. external libraries).
    Named(String),
    /// Virtual inner class produced by a framework analyzer.
    VirtualInner { owner: ClassId, name: String },
}

impl Type {
    pub fn boolean() -> Self {
        Self::Primitive(PrimitiveType::Boolean)
    }

    pub fn int() -> Self {
        Self::Primitive(PrimitiveType::Int)
    }

    pub fn is_primitive_boolean(&self) -> bool {
        matches!(self, Type::Primitive(PrimitiveType::Boolean))
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

// --- External type stubs ---------------------------------------------------
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
