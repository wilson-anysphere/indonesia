//! Core shared types for Nova.
//!
//! This crate is intentionally small and dependency-free.

use std::fmt;
use std::path::PathBuf;

/// The current Nova version.
///
/// Used for on-disk cache compatibility checks (indexes, caches, metadata).
pub const NOVA_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 1-based line number in a source file.
///
/// Nova uses different coordinate systems depending on the integration point:
/// - LSP uses 0-based lines/characters (`Position` below).
/// - DAP uses 1-based lines/columns (breakpoints, stack traces).
///
/// `Line` is a small convenience alias used by debugger-adjacent code.
pub type Line = u32;

/// 0-based column number in a source file.
pub type Column = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LineColumn {
    pub line: Line,
    pub column: Column,
}

impl LineColumn {
    #[inline]
    pub const fn new(line: Line, column: Column) -> Self {
        Self { line, column }
    }
}

/// A position in a text document expressed as (line, UTF-16 code unit offset).
///
/// This matches the Language Server Protocol definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

impl Position {
    #[inline]
    pub const fn new(line: u32, character: u32) -> Self {
        Self { line, character }
    }
}

/// A half-open range in a text document expressed with LSP positions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

impl Range {
    #[inline]
    pub const fn new(start: Position, end: Position) -> Self {
        Self { start, end }
    }
}

/// A textual edit described by a range replacement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEdit {
    pub range: Range,
    pub new_text: String,
}

impl TextEdit {
    #[inline]
    pub fn new(range: Range, new_text: impl Into<String>) -> Self {
        Self {
            range,
            new_text: new_text.into(),
        }
    }
}

/// A simple (unqualified) identifier.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Name(String);

impl Name {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Name").field(&self.0).finish()
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl From<&str> for Name {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for Name {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// A dotted package name, e.g. `java.lang` or `com.example`.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct PackageName(Vec<Name>);

impl PackageName {
    pub fn root() -> Self {
        Self(Vec::new())
    }

    pub fn segments(&self) -> &[Name] {
        &self.0
    }

    pub fn from_dotted(path: &str) -> Self {
        if path.is_empty() {
            return Self::root();
        }
        Self(path.split('.').map(Name::from).collect())
    }

    pub fn push(&mut self, seg: Name) {
        self.0.push(seg);
    }

    pub fn to_dotted(&self) -> String {
        self.0
            .iter()
            .map(|n| n.as_str())
            .collect::<Vec<_>>()
            .join(".")
    }
}

impl fmt::Debug for PackageName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("PackageName").field(&self.to_dotted()).finish()
    }
}

impl fmt::Display for PackageName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.to_dotted().fmt(f)
    }
}

/// A dotted type name (fully qualified).
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct QualifiedName(Vec<Name>);

impl QualifiedName {
    pub fn segments(&self) -> &[Name] {
        &self.0
    }

    pub fn from_dotted(path: &str) -> Self {
        Self(path.split('.').map(Name::from).collect())
    }

    pub fn to_dotted(&self) -> String {
        self.0
            .iter()
            .map(|n| n.as_str())
            .collect::<Vec<_>>()
            .join(".")
    }

    pub fn last(&self) -> Option<&Name> {
        self.0.last()
    }
}

impl fmt::Debug for QualifiedName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("QualifiedName")
            .field(&self.to_dotted())
            .finish()
    }
}

/// A resolved type identifier.
///
/// For now this is the fully qualified Java name, e.g. `java.lang.String`.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct TypeId(String);

impl TypeId {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for TypeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("TypeId").field(&self.0).finish()
    }
}

impl fmt::Display for TypeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl From<&str> for TypeId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

/// A resolved package identifier.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct PackageId(String);

impl PackageId {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for PackageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("PackageId").field(&self.0).finish()
    }
}

/// A very small abstraction over a source of types (JDK, project classpath, etc).
pub trait TypeIndex {
    /// Resolve a fully qualified type name.
    fn resolve_type(&self, name: &QualifiedName) -> Option<TypeId>;

    /// Resolve a type by package + simple name.
    fn resolve_type_in_package(&self, package: &PackageName, name: &Name) -> Option<TypeId>;

    /// Resolve a package (used for qualified-name resolution where intermediate segments may be packages).
    fn package_exists(&self, package: &PackageName) -> bool {
        let _ = package;
        false
    }

    /// Resolve a static field or method member on a type.
    fn resolve_static_member(&self, owner: &TypeId, name: &Name) -> Option<StaticMemberId> {
        let _ = (owner, name);
        None
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct StaticMemberId(String);

impl StaticMemberId {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for StaticMemberId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("StaticMemberId").field(&self.0).finish()
    }
}

/// Workspace/project-level configuration.
///
/// This is intentionally minimal for now. Downstream crates (e.g. the resolver)
/// can grow this as Nova's configuration model evolves.
#[derive(Clone, Debug, Default)]
pub struct ProjectConfig {
    /// Optional override for the JDK installation to use.
    pub jdk_home: Option<PathBuf>,
}
