use crate::parser::SyntaxNode;
use crate::syntax_kind::SyntaxKind;

pub trait AstNode: Sized {
    fn can_cast(kind: SyntaxKind) -> bool;
    fn cast(syntax: SyntaxNode) -> Option<Self>;
    fn syntax(&self) -> &SyntaxNode;
}

pub mod support {
    use crate::ast::AstNode;
    use crate::parser::{SyntaxNode, SyntaxToken};
    use crate::syntax_kind::SyntaxKind;

    pub fn child<N: AstNode>(node: &SyntaxNode) -> Option<N> {
        node.children().find_map(N::cast)
    }

    pub fn children<'a, N: AstNode + 'a>(node: &'a SyntaxNode) -> impl Iterator<Item = N> + 'a {
        node.children().filter_map(N::cast)
    }

    pub fn token(node: &SyntaxNode, kind: SyntaxKind) -> Option<SyntaxToken> {
        node.children_with_tokens()
            .filter_map(|it| it.into_token())
            .find(|tok| tok.kind() == kind)
    }

    pub fn tokens<'a>(
        node: &'a SyntaxNode,
        kind: SyntaxKind,
    ) -> impl Iterator<Item = SyntaxToken> + 'a {
        node.children_with_tokens()
            .filter_map(|it| it.into_token())
            .filter(move |tok| tok.kind() == kind)
    }

    /// Returns an identifier-like token among the node's direct children.
    pub fn ident_token(node: &SyntaxNode) -> Option<SyntaxToken> {
        // Java has a number of "contextual keywords" (e.g. `record`, `yield`, `var`) which the
        // lexer still classifies as dedicated token kinds. For many syntax nodes (notably record
        // declarations), those keywords appear before the actual identifier token.
        //
        // Prefer the last identifier-like token among the node's *direct* children, which is
        // typically the declared name in Nova's current tree shapes.
        ident_tokens(node).last()
    }

    pub fn ident_tokens(node: &SyntaxNode) -> impl Iterator<Item = SyntaxToken> + '_ {
        node.children_with_tokens()
            .filter_map(|it| it.into_token())
            .filter(|tok| tok.kind().is_identifier_like())
    }
}

mod generated;

pub use generated::*;

mod ext;

#[cfg(test)]
mod tests;

impl CompilationUnit {
    /// Compatibility accessor returning the raw syntax nodes for the top-level type declarations.
    ///
    /// Prefer [`CompilationUnit::type_declarations`] for typed access.
    pub fn types(&self) -> impl Iterator<Item = SyntaxNode> + '_ {
        self.syntax().children().filter(|n| {
            matches!(
                n.kind(),
                SyntaxKind::ClassDeclaration
                    | SyntaxKind::InterfaceDeclaration
                    | SyntaxKind::EnumDeclaration
                    | SyntaxKind::RecordDeclaration
                    | SyntaxKind::AnnotationTypeDeclaration
            )
        })
    }
}
