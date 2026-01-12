use crate::edit::{FileId, TextRange};
use crate::java::{JavaSymbolKind, SymbolId};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MethodSignature {
    pub param_types: Vec<String>,
}

impl MethodSignature {
    #[must_use]
    pub fn arity(&self) -> usize {
        self.param_types.len()
    }
}

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
    /// Introducing a new local would shadow an existing field with the same name,
    /// changing later unqualified field accesses (e.g. `value` vs `this.value`).
    FieldShadowing {
        file: FileId,
        name: String,
        usage_range: TextRange,
    },
    /// A rename would cause a specific usage site to resolve to a different symbol.
    ///
    /// This is most commonly triggered when renaming a non-local symbol (e.g. a field) to a name
    /// that is already bound in the local scope at a particular reference site.
    ReferenceWillChangeResolution {
        file: FileId,
        usage_range: TextRange,
        name: String,
        existing_symbol: SymbolId,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReferenceKind {
    /// A simple identifier reference originating from `hir::Expr::Name`.
    ///
    /// These are subject to name capture when a rename introduces a local/parameter binding with
    /// the same name.
    Name,
    /// A qualified member reference originating from `hir::Expr::FieldAccess.name_range` (e.g.
    /// `this.foo`).
    ///
    /// Qualified references are not subject to local-variable name capture.
    FieldAccess,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reference {
    pub file: FileId,
    pub range: TextRange,
    /// The interned scope ID in which this reference occurs (when available).
    pub scope: Option<u32>,
    pub kind: ReferenceKind,
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

    /// Best-effort symbol lookup at a given byte offset.
    ///
    /// The default implementation returns `None`. Semantic databases (like
    /// [`crate::java::RefactorJavaDatabase`]) can override this to support
    /// refactorings that need to resolve identifiers within an expression.
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
    fn resolve_field_in_scope(&self, _scope: u32, _name: &str) -> Option<SymbolId> {
        None
    }
    fn resolve_methods_in_scope(&self, _scope: u32, _name: &str) -> Vec<SymbolId> {
        Vec::new()
    }
    fn method_signature(&self, _symbol: SymbolId) -> Option<MethodSignature> {
        None
    }
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

    /// Resolve the symbol for a name expression at a given byte range.
    ///
    /// This is primarily used by refactorings (e.g. Inline Variable) to validate that a
    /// text range still semantically refers to the intended [`SymbolId`], even in the
    /// presence of shadowing / identical identifier text.
    ///
    /// Implementations should return `Some(SymbolId)` only when `range` corresponds to an
    /// identifier expression node (e.g. `ast::NameExpression`) and name resolution at that
    /// location succeeds.
    fn resolve_name_expr(&self, _file: &FileId, _range: TextRange) -> Option<SymbolId> {
        None
    }

    /// Return the full override/implementation chain for a method symbol.
    ///
    /// The returned list should include `symbol` itself. Implementations are free to return
    /// an empty list if the symbol is unknown or not a method.
    ///
    /// Note: the default implementation returns just `symbol`, which preserves the existing
    /// behaviour of renaming a single method declaration.
    fn method_override_chain(&self, symbol: SymbolId) -> Vec<SymbolId> {
        vec![symbol]
    }

    /// Best-effort visibility check.
    ///
    /// The default implementation assumes visibility is preserved.
    fn is_visible_from(&self, _symbol: SymbolId, _from_file: &FileId, _new_name: &str) -> bool {
        true
    }
}
