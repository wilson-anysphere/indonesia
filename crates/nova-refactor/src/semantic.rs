use crate::edit::{FileId, TextRange};
use crate::java::{JavaSymbolKind, SymbolId};

/// Semantic representation of program changes.
///
/// The intent is that refactorings describe changes in terms of semantic
/// operations ("rename symbol X") and later materialize them into concrete text
/// edits. Only a subset is currently implemented end-to-end; the full enum is
/// provided so higher-level refactorings can be expressed consistently.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SemanticChange {
    /// Rename a symbol and update all semantic references.
    Rename { symbol: SymbolId, new_name: String },

    /// Move a declaration from one file/range to another location.
    ///
    /// This is modeled as a semantic operation but materializes into a delete
    /// + insert edit pair.
    Move {
        file: FileId,
        range: TextRange,
        target_file: FileId,
        target_offset: usize,
    },

    /// Add text at a given location.
    Add {
        file: FileId,
        offset: usize,
        text: String,
    },

    /// Remove a text range.
    Remove { file: FileId, range: TextRange },

    /// Replace a range with text.
    Replace {
        file: FileId,
        range: TextRange,
        text: String,
    },

    /// Change a type annotation range.
    ChangeType {
        file: FileId,
        range: TextRange,
        new_type: String,
    },

    /// Update reference text directly.
    ///
    /// This variant is useful for transformations that cannot be expressed as a
    /// rename but still want to model "semantic references" explicitly.
    UpdateReferences {
        file: FileId,
        range: TextRange,
        new_text: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Conflict {
    NameCollision {
        file: FileId,
        name: String,
        existing_symbol: SymbolId,
    },
    Shadowing {
        file: FileId,
        name: String,
        shadowed_symbol: SymbolId,
    },
    VisibilityLoss {
        file: FileId,
        usage_range: TextRange,
        name: String,
    },
    FileAlreadyExists {
        file: FileId,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SymbolDefinition {
    pub file: FileId,
    pub name: String,
    pub name_range: TextRange,
    pub scope: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reference {
    pub file: FileId,
    pub range: TextRange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypeSymbolInfo {
    /// Dotted Java package name (`com.example`). `None` represents the default package.
    pub package: Option<String>,
    pub is_top_level: bool,
    pub is_public: bool,
}

/// Abstraction over Nova's semantic database/index.
///
/// The production implementation is backed by Nova's canonical semantic crates
/// (`nova-syntax` + `nova-hir` + `nova-resolve` via Salsa). Fixture tests can
/// construct a lightweight database from in-memory file contents.
pub trait RefactorDatabase {
    fn file_text(&self, file: &FileId) -> Option<&str>;

    /// Enumerate all known workspace files.
    ///
    /// Multi-file refactorings (e.g. Java package moves) use this to build an in-memory workspace
    /// snapshot.
    ///
    /// The default implementation returns an empty list so lightweight database adapters (like
    /// `nova_index::Index`) do not need to materialize a full file set.
    ///
    /// This is also used for refactorings that need to find syntactic constructs which are not
    /// represented as explicit semantic references (for example, Java's annotation shorthand
    /// `@Anno(expr)` for `value()`).
    fn all_files(&self) -> Vec<FileId> {
        Vec::new()
    }

    fn symbol_at(&self, _file: &FileId, _offset: usize) -> Option<SymbolId> {
        None
    }

    fn file_exists(&self, file: &FileId) -> bool {
        self.file_text(file).is_some()
    }

    fn symbol_definition(&self, symbol: SymbolId) -> Option<SymbolDefinition>;
    fn symbol_scope(&self, symbol: SymbolId) -> Option<u32>;
    fn symbol_kind(&self, _symbol: SymbolId) -> Option<JavaSymbolKind> {
        None
    }

    fn type_symbol_info(&self, _symbol: SymbolId) -> Option<TypeSymbolInfo> {
        None
    }

    fn find_top_level_type_in_package(
        &self,
        _package: Option<&str>,
        _name: &str,
    ) -> Option<SymbolId> {
        None
    }

    fn resolve_name_in_scope(&self, scope: u32, name: &str) -> Option<SymbolId>;
    fn would_shadow(&self, scope: u32, name: &str) -> Option<SymbolId>;

    fn find_references(&self, symbol: SymbolId) -> Vec<Reference>;

    /// Best-effort type inference helper.
    ///
    /// Implementations backed by Nova's Salsa database can provide a richer type display string
    /// (including generics) than parser-only heuristics.
    ///
    /// The default implementation returns `None` to preserve compatibility for lightweight
    /// databases (e.g. `TextDatabase`, `Index`).
    fn type_at_offset_display(&self, _file: &FileId, _offset: usize) -> Option<String> {
        None
    }

    /// Best-effort visibility check.
    ///
    /// The default implementation assumes visibility is preserved.
    fn is_visible_from(&self, _symbol: SymbolId, _from_file: &FileId, _new_name: &str) -> bool {
        true
    }
}
