//! High-level intermediate representation (HIR) for Java.
//!
//! The real Nova project will have a much richer HIR. For now this is just
//! enough structure to build scope graphs and test name resolution.

use nova_core::{Name, PackageName, QualifiedName};

#[derive(Debug, Clone)]
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

#[derive(Debug, Clone)]
pub enum ImportDecl {
    TypeSingle {
        ty: QualifiedName,
        alias: Option<Name>,
    },
    TypeStar {
        package: PackageName,
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

#[derive(Debug, Clone)]
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

#[derive(Debug, Clone)]
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

#[derive(Debug, Clone)]
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

#[derive(Debug, Clone)]
pub struct ParamDecl {
    pub name: Name,
}

impl ParamDecl {
    pub fn new(name: impl Into<Name>) -> Self {
        Self { name: name.into() }
    }
}

#[derive(Debug, Clone)]
pub struct Block {
    pub stmts: Vec<Stmt>,
}

#[derive(Debug, Clone)]
pub enum Stmt {
    Local(LocalVarDecl),
    Block(Block),
}

#[derive(Debug, Clone)]
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
    use nova_types::{Parameter, Type};

    #[derive(Debug, Clone, PartialEq, Eq, Hash)]
    pub struct Annotation {
        pub name: String,
    }

    impl Annotation {
        pub fn new(name: impl Into<String>) -> Self {
            let mut name = name.into();
            if let Some(stripped) = name.strip_prefix('@') {
                name = stripped.to_string();
            }
            Self { name }
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

    #[derive(Debug, Clone, PartialEq, Eq)]
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
    }

    impl Default for ClassData {
        fn default() -> Self {
            Self {
                name: String::new(),
                annotations: Vec::new(),
                fields: Vec::new(),
                methods: Vec::new(),
                constructors: Vec::new(),
            }
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

// ---------------------------------------------------------------------
// ItemTree (persisted, per-file structural summary)

use nova_syntax::{ParseResult, SyntaxKind, TextRange};
use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize_repr, Deserialize_repr,
)]
#[repr(u8)]
pub enum ItemKind {
    Class = 0,
    Interface = 1,
    Enum = 2,
    Record = 3,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub kind: ItemKind,
    pub name: String,
    pub name_range: TextRange,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemTree {
    pub items: Vec<Item>,
}

impl ItemTree {
    pub fn empty() -> Self {
        Self { items: Vec::new() }
    }
}

/// Optional per-file symbol summary used by indexing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolSummary {
    pub names: Vec<String>,
}

impl SymbolSummary {
    pub fn from_item_tree(item_tree: &ItemTree) -> Self {
        Self {
            names: item_tree.items.iter().map(|it| it.name.clone()).collect(),
        }
    }
}

fn token_text<'a>(text: &'a str, range: TextRange) -> &'a str {
    let start = range.start as usize;
    let end = range.end as usize;
    &text[start..end]
}

fn is_trivia(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::Whitespace
            | SyntaxKind::LineComment
            | SyntaxKind::BlockComment
            | SyntaxKind::DocComment
    )
}

/// Build a per-file `ItemTree` from a flat token stream.
///
/// The current implementation uses a tiny recognizer (it looks for `class`,
/// `interface`, `enum`, `record` followed by an identifier). The full Nova parser
/// will replace this with a real Java grammar while keeping the persisted
/// `ItemTree` format stable via schema versioning.
pub fn item_tree(parse: &ParseResult, text: &str) -> ItemTree {
    let tokens: Vec<_> = parse.tokens().collect();
    let mut items = Vec::new();
    let mut i = 0usize;

    while i < tokens.len() {
        let tok = tokens[i];
        if tok.kind != SyntaxKind::Identifier {
            i += 1;
            continue;
        }

        let kw = token_text(text, tok.range);
        let (kind, is_decl) = match kw {
            "class" => (ItemKind::Class, true),
            "interface" => (ItemKind::Interface, true),
            "enum" => (ItemKind::Enum, true),
            "record" => (ItemKind::Record, true),
            _ => (ItemKind::Class, false),
        };

        if !is_decl {
            i += 1;
            continue;
        }

        // Find the next non-trivia token.
        let mut j = i + 1;
        while j < tokens.len() && is_trivia(tokens[j].kind) {
            j += 1;
        }

        if j < tokens.len() && tokens[j].kind == SyntaxKind::Identifier {
            let name_tok = tokens[j];
            items.push(Item {
                kind,
                name: token_text(text, name_tok.range).to_string(),
                name_range: name_tok.range,
            });
        }

        i = j + 1;
    }

    ItemTree { items }
}

// ---------------------------------------------------------------------
// Experimental semantic substrate (`ItemTree` + body HIR with stable IDs)

pub mod hir;
pub mod ids;
pub mod item_tree;
pub mod lowering;
pub mod module_info;
pub mod queries;
