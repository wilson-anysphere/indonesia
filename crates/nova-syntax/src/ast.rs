use crate::parser::SyntaxNode;
use crate::syntax_kind::SyntaxKind;

pub trait AstNode: Sized {
    fn can_cast(kind: SyntaxKind) -> bool;
    fn cast(syntax: SyntaxNode) -> Option<Self>;
    fn syntax(&self) -> &SyntaxNode;
}

macro_rules! ast_node {
    ($name:ident, $kind:path) => {
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub struct $name {
            syntax: SyntaxNode,
        }

        impl AstNode for $name {
            fn can_cast(kind: SyntaxKind) -> bool {
                kind == $kind
            }

            fn cast(syntax: SyntaxNode) -> Option<Self> {
                Self::can_cast(syntax.kind()).then_some(Self { syntax })
            }

            fn syntax(&self) -> &SyntaxNode {
                &self.syntax
            }
        }
    };
}

ast_node!(CompilationUnit, SyntaxKind::CompilationUnit);
ast_node!(PackageDeclaration, SyntaxKind::PackageDeclaration);
ast_node!(ImportDeclaration, SyntaxKind::ImportDeclaration);
ast_node!(ClassDeclaration, SyntaxKind::ClassDeclaration);
ast_node!(InterfaceDeclaration, SyntaxKind::InterfaceDeclaration);
ast_node!(EnumDeclaration, SyntaxKind::EnumDeclaration);
ast_node!(RecordDeclaration, SyntaxKind::RecordDeclaration);
ast_node!(MethodDeclaration, SyntaxKind::MethodDeclaration);
ast_node!(FieldDeclaration, SyntaxKind::FieldDeclaration);

impl CompilationUnit {
    pub fn package(&self) -> Option<PackageDeclaration> {
        self.syntax.children().find_map(PackageDeclaration::cast)
    }

    pub fn imports(&self) -> impl Iterator<Item = ImportDeclaration> + '_ {
        self.syntax.children().filter_map(ImportDeclaration::cast)
    }

    pub fn types(&self) -> impl Iterator<Item = SyntaxNode> + '_ {
        self.syntax.children().filter(|n| {
            matches!(
                n.kind(),
                SyntaxKind::ClassDeclaration
                    | SyntaxKind::InterfaceDeclaration
                    | SyntaxKind::EnumDeclaration
                    | SyntaxKind::RecordDeclaration
            )
        })
    }
}

