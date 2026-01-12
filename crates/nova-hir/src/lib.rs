//! High-level intermediate representation (HIR) for Java.
//!
//! The real Nova project will have a much richer HIR. For now this is just
//! enough structure to build scope graphs and test name resolution.

use nova_core::{Name, PackageName, QualifiedName};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompilationUnit {
    pub package: Option<PackageName>,
    pub imports: Vec<ImportDecl>,
    pub types: Vec<TypeDecl>,
}

impl CompilationUnit {
    pub fn new(package: Option<PackageName>) -> Self {
        Self {
            package,
            imports: Vec::new(),
            types: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportDecl {
    TypeSingle {
        ty: QualifiedName,
        alias: Option<Name>,
    },
    TypeStar {
        /// `import X.*;` where `X` is a `PackageOrTypeName` (JLS 7.5.2).
        ///
        /// `X` can refer to either:
        /// - a package (`import java.util.*;`), or
        /// - a type (`import java.util.Map.*;`), in which case member types can be imported.
        ///
        /// Callers can interpret this via `TypeIndex::package_exists` and/or by resolving `X`
        /// as a type name.
        path: QualifiedName,
    },
    StaticSingle {
        ty: QualifiedName,
        member: Name,
        alias: Option<Name>,
    },
    StaticStar {
        ty: QualifiedName,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeDecl {
    pub name: Name,
    pub fields: Vec<FieldDecl>,
    pub methods: Vec<MethodDecl>,
    pub nested_types: Vec<TypeDecl>,
}

impl TypeDecl {
    pub fn new(name: impl Into<Name>) -> Self {
        Self {
            name: name.into(),
            fields: Vec::new(),
            methods: Vec::new(),
            nested_types: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldDecl {
    pub name: Name,
    pub is_static: bool,
}

impl FieldDecl {
    pub fn new(name: impl Into<Name>) -> Self {
        Self {
            name: name.into(),
            is_static: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodDecl {
    pub name: Name,
    pub is_static: bool,
    pub params: Vec<ParamDecl>,
    pub body: Block,
}

impl MethodDecl {
    pub fn new(name: impl Into<Name>) -> Self {
        Self {
            name: name.into(),
            is_static: false,
            params: Vec::new(),
            body: Block { stmts: Vec::new() },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamDecl {
    pub name: Name,
}

impl ParamDecl {
    pub fn new(name: impl Into<Name>) -> Self {
        Self { name: name.into() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub stmts: Vec<Stmt>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stmt {
    Local(LocalVarDecl),
    Block(Block),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalVarDecl {
    pub name: Name,
}

impl LocalVarDecl {
    pub fn new(name: impl Into<Name>) -> Self {
        Self { name: name.into() }
    }
}

/// Additional HIR data structures used by framework analyzers (e.g. Lombok).
///
/// The core HIR in this crate is currently focused on scope building/name
/// resolution. Frameworks frequently need a richer, annotation-aware view of a
/// class even when we are not running annotation processors.
pub mod framework {
    use nova_types::{Parameter, Span, Type};

    #[derive(Debug, Clone, PartialEq, Eq, Hash)]
    pub struct Annotation {
        pub name: String,
        pub span: Option<Span>,
    }

    impl Annotation {
        pub fn new(name: impl Into<String>) -> Self {
            let mut name = name.into();
            if let Some(stripped) = name.strip_prefix('@') {
                name = stripped.to_string();
            }
            Self { name, span: None }
        }

        pub fn new_with_span(name: impl Into<String>, span: Span) -> Self {
            let mut annotation = Self::new(name);
            annotation.span = Some(span);
            annotation
        }

        pub fn matches(&self, query: &str) -> bool {
            annotation_matches(&self.name, query)
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct FieldData {
        pub name: String,
        pub ty: Type,
        pub is_static: bool,
        pub is_final: bool,
        pub annotations: Vec<Annotation>,
    }

    impl FieldData {
        pub fn has_annotation(&self, name: &str) -> bool {
            self.annotations.iter().any(|a| a.matches(name))
        }

        pub fn annotation_span(&self, name: &str) -> Option<Span> {
            self.annotations
                .iter()
                .find(|a| a.matches(name))
                .and_then(|a| a.span)
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct MethodData {
        pub name: String,
        pub return_type: Type,
        pub params: Vec<Parameter>,
        pub is_static: bool,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ConstructorData {
        pub params: Vec<Parameter>,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Default)]
    pub struct ClassData {
        pub name: String,
        pub annotations: Vec<Annotation>,
        pub fields: Vec<FieldData>,
        pub methods: Vec<MethodData>,
        pub constructors: Vec<ConstructorData>,
    }

    impl ClassData {
        pub fn has_annotation(&self, name: &str) -> bool {
            self.annotations.iter().any(|a| a.matches(name))
        }

        pub fn annotation_span(&self, name: &str) -> Option<Span> {
            self.annotations
                .iter()
                .find(|a| a.matches(name))
                .and_then(|a| a.span)
        }
    }

    fn annotation_matches(annotation: &str, query: &str) -> bool {
        if annotation == query {
            return true;
        }
        let annotation_simple = annotation.rsplit('.').next().unwrap_or(annotation);
        let query_simple = query.rsplit('.').next().unwrap_or(query);
        annotation_simple == query_simple
    }
}

/// Flow-oriented method-body IR used by `nova-flow`.
pub mod body;
pub mod body_lowering;

// ---------------------------------------------------------------------
// Token-based per-file summary (early-cutoff demo).

/// Version of the on-disk token-HIR schema used by `nova-cache`.
///
/// Bump this whenever the serialized `TokenItemTree` / `TokenSymbolSummary`
/// format changes in an incompatible way.
pub const HIR_SCHEMA_VERSION: u32 = 1;

pub mod token_item_tree;

// ---------------------------------------------------------------------
// Experimental semantic substrate (`ItemTree` + body HIR with stable IDs)

pub mod ast_id;
pub mod hir;
pub mod ids;
pub mod item_tree;
pub mod lowering;
pub mod module_info;
pub mod queries;

#[cfg(test)]
mod tests;
