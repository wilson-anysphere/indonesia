//! Shared, dependency-minimized core types used across Nova.

pub mod diagnostic;
pub mod edit;
pub mod id;
pub mod name;
pub mod path;
pub mod text;

pub mod fs;

use std::fmt;
use std::path::{Path, PathBuf};

/// The current Nova version.
///
/// Used for on-disk cache compatibility checks (indexes, caches, metadata).
pub const NOVA_VERSION: &str = env!("CARGO_PKG_VERSION");

pub use diagnostic::{Location, RelatedDiagnostic, Severity};
pub use edit::{apply_text_edits, normalize_text_edits, EditError, TextEdit, WorkspaceEdit};
pub use id::*;
pub use name::{InternedName, Name, NameInterner, SymbolName};
pub use path::{file_uri_to_path, path_to_file_uri, AbsPathBuf, PathToUriError, UriToPathError};
pub use text::{LineCol, LineIndex, Position, Range, TextRange, TextSize};

#[cfg(feature = "lsp")]
pub use path::{lsp_uri_to_path, path_to_lsp_uri};

/// 1-based line number in a source file.
///
/// Nova uses different coordinate systems depending on the integration point:
/// - LSP uses 0-based lines/characters (`Position`).
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
        f.debug_tuple("PackageName")
            .field(&self.to_dotted())
            .finish()
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
/// In the full Nova project this will likely be a stable numeric ID.
/// For the current prototype, it's represented as the fully-qualified Java name
/// (e.g. `java.lang.String`).
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct TypeName(String);

impl TypeName {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for TypeName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("TypeName").field(&self.0).finish()
    }
}

impl fmt::Display for TypeName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl From<&str> for TypeName {
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
    fn resolve_type(&self, name: &QualifiedName) -> Option<TypeName>;

    /// Resolve a type by package + simple name.
    fn resolve_type_in_package(&self, package: &PackageName, name: &Name) -> Option<TypeName>;

    /// Resolve a package (used for qualified-name resolution where intermediate segments may be packages).
    fn package_exists(&self, package: &PackageName) -> bool {
        let _ = package;
        false
    }

    /// Resolve a static field or method member on a type.
    fn resolve_static_member(&self, owner: &TypeName, name: &Name) -> Option<StaticMemberId> {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

/// A diagnostic produced by parsing/analysis/build tooling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// File the diagnostic applies to.
    pub file: PathBuf,
    pub range: Range,
    pub severity: DiagnosticSeverity,
    pub message: String,
    pub source: Option<String>,
}

impl Diagnostic {
    pub fn new(
        file: PathBuf,
        range: Range,
        severity: DiagnosticSeverity,
        message: impl Into<String>,
        source: impl Into<Option<String>>,
    ) -> Self {
        Self {
            file,
            range,
            severity,
            message: message.into(),
            source: source.into(),
        }
    }
}

// -----------------------------------------------------------------------------
// AI scaffolding surfaces (used by `nova-ai` and integration layers)
// -----------------------------------------------------------------------------

/// A single completion candidate produced by Nova's non-AI completion engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionItem {
    pub label: String,
    pub kind: CompletionItemKind,
}

impl CompletionItem {
    pub fn new(label: impl Into<String>, kind: CompletionItemKind) -> Self {
        Self {
            label: label.into(),
            kind,
        }
    }
}

/// A coarse classification for completion items.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum CompletionItemKind {
    Keyword,
    Class,
    Method,
    Field,
    Variable,
    Snippet,
    Other,
}

/// Context used for AI-assisted completion ranking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionContext {
    /// What the user has already typed.
    pub prefix: String,

    /// The current line text (optional; used for heuristics).
    pub line_text: String,
}

impl CompletionContext {
    pub fn new(prefix: impl Into<String>, line_text: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
            line_text: line_text.into(),
        }
    }
}

/// Abstract view of a project used by indexing/search subsystems.
///
/// In the full Nova architecture this will likely be backed by a query database.
/// For now, we only expose what the AI scaffolding needs.
pub trait ProjectDatabase {
    /// Return the list of project files that should be searchable.
    fn project_files(&self) -> Vec<PathBuf>;

    /// Return the UTF-8 text contents for a given file.
    fn file_text(&self, path: &Path) -> Option<String>;
}

/// Identifier used to refer to a symbol within in-memory indexes.
pub type SymbolId = u32;
