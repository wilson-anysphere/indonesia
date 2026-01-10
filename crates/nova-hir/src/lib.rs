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
