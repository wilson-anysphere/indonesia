//! Shared, dependency-minimized core types used across Nova.

pub mod debug_config;
pub mod edit;
pub mod id;
pub mod name;
pub mod path;
pub mod text;

pub mod fs;

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

/// The current Nova version.
///
/// Used for on-disk cache compatibility checks (indexes, caches, metadata).
pub const NOVA_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Endianness identifier used by persisted artifacts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Endian {
    Little = 0,
    Big = 1,
}

/// Returns the target endianness of the current build.
#[inline]
pub const fn target_endian() -> Endian {
    if cfg!(target_endian = "little") {
        Endian::Little
    } else if cfg!(target_endian = "big") {
        Endian::Big
    } else {
        Endian::Little
    }
}

/// Returns the target pointer width (32/64) of the current build.
#[inline]
pub const fn target_pointer_width() -> u8 {
    if cfg!(target_pointer_width = "64") {
        64
    } else if cfg!(target_pointer_width = "32") {
        32
    } else {
        0
    }
}

pub use debug_config::{AttachConfig, LaunchConfig};
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

impl std::borrow::Borrow<str> for TypeName {
    fn borrow(&self) -> &str {
        self.as_str()
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

    /// Enumerate known static members for `owner`.
    ///
    /// This is primarily used for member completion (e.g. `Math.<cursor>`).
    ///
    /// Implementations are expected to return a *best-effort* list: returning an
    /// empty list is always valid. Callers should treat the result as incomplete.
    fn static_members(&self, owner: &TypeName) -> Vec<StaticMemberInfo> {
        let _ = owner;
        Vec::new()
    }
}

/// A coarse kind for a static member (sufficient for completion item shaping).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaticMemberKind {
    Method,
    Field,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticMemberInfo {
    pub name: Name,
    pub kind: StaticMemberKind,
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

/// Configuration inputs for JDK discovery.
///
/// This is intentionally minimal for now, but avoids confusion with
/// `nova_project::ProjectConfig` (the build graph + source roots).
#[derive(Clone, Debug, Default)]
pub struct JdkConfig {
    /// Optional override for the JDK installation to use.
    pub home: Option<PathBuf>,

    /// Default Java feature release used for `--release`-style API selection when callers don't
    /// provide one explicitly.
    pub release: Option<u16>,

    /// Per-release JDK installation overrides.
    ///
    /// Keys are Java release numbers (e.g. 8, 11, 17). When multiple toolchains are configured
    /// for the same release in a higher-level config, the last one wins.
    pub toolchains: BTreeMap<u16, PathBuf>,
}

impl JdkConfig {
    /// Returns the configured toolchain home for `release` if one exists.
    pub fn toolchain_home_for_release(&self, release: u16) -> Option<&PathBuf> {
        self.toolchains.get(&release)
    }

    /// Returns the preferred JDK home for the requested API release.
    ///
    /// Resolution order:
    /// 1. If `requested_release` is `Some`, use it; otherwise fall back to `self.release`.
    /// 2. If a matching toolchain exists, return its home.
    /// 3. Otherwise return `self.home`.
    pub fn preferred_home(&self, requested_release: Option<u16>) -> Option<&PathBuf> {
        requested_release
            .or(self.release)
            .and_then(|release| self.toolchain_home_for_release(release))
            .or(self.home.as_ref())
    }
}

#[cfg(test)]
mod jdk_config_tests {
    use super::*;

    #[test]
    fn prefers_matching_toolchain_over_default_home() {
        let default_jdk = PathBuf::from("/default-jdk");
        let jdk_8 = PathBuf::from("/jdk-8");
        let jdk_17 = PathBuf::from("/jdk-17");

        let cfg = JdkConfig {
            home: Some(default_jdk.clone()),
            release: Some(17),
            toolchains: [(8u16, jdk_8.clone()), (17u16, jdk_17.clone())]
                .into_iter()
                .collect(),
        };

        assert_eq!(cfg.preferred_home(None), Some(&jdk_17));
        assert_eq!(cfg.preferred_home(Some(8)), Some(&jdk_8));
        assert_eq!(cfg.preferred_home(Some(11)), Some(&default_jdk));
    }

    #[test]
    fn last_toolchain_wins_for_duplicate_releases() {
        let jdk_17_a = PathBuf::from("/jdk-17-a");
        let jdk_17_b = PathBuf::from("/jdk-17-b");

        let mut toolchains: BTreeMap<u16, PathBuf> = BTreeMap::new();
        toolchains.insert(17, jdk_17_a);
        toolchains.insert(17, jdk_17_b.clone());

        let cfg = JdkConfig {
            home: None,
            release: Some(17),
            toolchains,
        };

        assert_eq!(cfg.toolchain_home_for_release(17), Some(&jdk_17_b));
        assert_eq!(cfg.preferred_home(None), Some(&jdk_17_b));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuildDiagnosticSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

/// A diagnostic produced by external tools (build systems, BSP servers, etc).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildDiagnostic {
    /// File the diagnostic applies to.
    pub file: PathBuf,
    pub range: Range,
    pub severity: BuildDiagnosticSeverity,
    pub message: String,
    pub source: Option<String>,
}

impl BuildDiagnostic {
    pub fn new(
        file: PathBuf,
        range: Range,
        severity: BuildDiagnosticSeverity,
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
// Extension host surfaces (used by `nova-ext` and integration layers)
// -----------------------------------------------------------------------------

/// Minimal host-side database access required to construct requests for Nova WASM extensions.
///
/// This trait lives in `nova-core` (rather than `nova-ext`) to avoid introducing a forbidden
/// dependency edge from `nova-ext` (core layer) to `nova-db` (semantic layer). The primary
/// database trait (`nova_db::Database`) implements this for its `dyn` object type.
pub trait WasmHostDb {
    /// Return the current UTF-8 text for `file`.
    fn file_text(&self, file: FileId) -> &str;

    /// Best-effort file path lookup for `file`.
    fn file_path(&self, _file: FileId) -> Option<&Path> {
        None
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
    /// Optional extra metadata for the completion item.
    ///
    /// This is typically a type or symbol signature (e.g. method overload params/return type) and
    /// is primarily used by AI-backed completion ranking to disambiguate otherwise similar items.
    pub detail: Option<String>,
}

impl CompletionItem {
    pub fn new(label: impl Into<String>, kind: CompletionItemKind) -> Self {
        Self {
            label: label.into(),
            kind,
            detail: None,
        }
    }

    #[inline]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
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

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum RequestId {
    Number(i64),
    String(Box<str>),
}

impl From<i64> for RequestId {
    fn from(value: i64) -> Self {
        Self::Number(value)
    }
}

impl From<String> for RequestId {
    fn from(value: String) -> Self {
        Self::String(value.into_boxed_str())
    }
}

impl From<&str> for RequestId {
    fn from(value: &str) -> Self {
        Self::String(value.into())
    }
}
