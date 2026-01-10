//! Shared types used across Nova crates.
//!
//! The upstream Nova project has a much richer type system. In this kata repo we
//! keep this crate intentionally small and focused on what early framework
//! analyzers/tests need.

use std::fmt;

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

