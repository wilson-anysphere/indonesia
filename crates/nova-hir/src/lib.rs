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
