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

    /// Returns the first identifier-like token among the node's direct children.
    pub fn ident_token(node: &SyntaxNode) -> Option<SyntaxToken> {
        ident_tokens(node).next()
    }

    pub fn ident_tokens(node: &SyntaxNode) -> impl Iterator<Item = SyntaxToken> + '_ {
        node.children_with_tokens()
            .filter_map(|it| it.into_token())
            .filter(|tok| tok.kind().is_identifier_like())
    }
}

mod generated;

pub use generated::*;

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

impl SwitchLabel {
    /// Returns the constant-expression label elements (if any).
    ///
    /// Note: switch pattern labels are represented as [`CaseLabelElement`] nodes, so
    /// expressions are no longer direct children of the switch label.
    pub fn expressions(&self) -> impl Iterator<Item = Expression> + '_ {
        self.elements().filter_map(|el| el.expression())
    }
}

// --- JPMS module-info.java nodes ---------------------------------------------------------------

ast_node!(ModuleDeclaration, SyntaxKind::ModuleDeclaration);
ast_node!(ModuleBody, SyntaxKind::ModuleBody);
ast_node!(ModuleDirective, SyntaxKind::ModuleDirective);
ast_node!(RequiresDirective, SyntaxKind::RequiresDirective);
ast_node!(ExportsDirective, SyntaxKind::ExportsDirective);
ast_node!(OpensDirective, SyntaxKind::OpensDirective);
ast_node!(UsesDirective, SyntaxKind::UsesDirective);
ast_node!(ProvidesDirective, SyntaxKind::ProvidesDirective);

// Statements / expressions (common, non-exhaustive).
ast_node!(Block, SyntaxKind::Block);
ast_node!(IfStatement, SyntaxKind::IfStatement);
ast_node!(ForStatement, SyntaxKind::ForStatement);
ast_node!(WhileStatement, SyntaxKind::WhileStatement);
ast_node!(DoWhileStatement, SyntaxKind::DoWhileStatement);
ast_node!(TryStatement, SyntaxKind::TryStatement);
ast_node!(ReturnStatement, SyntaxKind::ReturnStatement);
ast_node!(ThrowStatement, SyntaxKind::ThrowStatement);
ast_node!(BreakStatement, SyntaxKind::BreakStatement);
ast_node!(ContinueStatement, SyntaxKind::ContinueStatement);
ast_node!(AssertStatement, SyntaxKind::AssertStatement);
ast_node!(YieldStatement, SyntaxKind::YieldStatement);
ast_node!(
    LocalVariableDeclarationStatement,
    SyntaxKind::LocalVariableDeclarationStatement
);
ast_node!(
    LocalTypeDeclarationStatement,
    SyntaxKind::LocalTypeDeclarationStatement
);
ast_node!(ExpressionStatement, SyntaxKind::ExpressionStatement);
ast_node!(EmptyStatement, SyntaxKind::EmptyStatement);

ast_node!(SwitchStatement, SyntaxKind::SwitchStatement);
ast_node!(SwitchExpression, SyntaxKind::SwitchExpression);
ast_node!(SwitchBlock, SyntaxKind::SwitchBlock);
ast_node!(SwitchGroup, SyntaxKind::SwitchGroup);
ast_node!(SwitchRule, SyntaxKind::SwitchRule);
ast_node!(SwitchLabel, SyntaxKind::SwitchLabel);

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
            )
        })
    }

    pub fn module_declaration(&self) -> Option<ModuleDeclaration> {
        support::child::<ModuleDeclaration>(self.syntax())
    }
}

impl Name {
    pub fn text(&self) -> String {
        let mut out = String::new();
        for tok in self
            .syntax()
            .children_with_tokens()
            .filter_map(|el| el.into_token())
            .filter(|tok| !tok.kind().is_trivia())
        {
            out.push_str(tok.text());
        }
        out
    }
}

impl ModuleDeclaration {
    pub fn name(&self) -> Option<Name> {
        support::child::<Name>(self.syntax())
    }

    pub fn body(&self) -> Option<ModuleBody> {
        support::child::<ModuleBody>(self.syntax())
    }

    pub fn is_open(&self) -> bool {
        support::token(self.syntax(), SyntaxKind::OpenKw).is_some()
    }
}

impl ModuleBody {
    pub fn directive_items(&self) -> impl Iterator<Item = ModuleDirective> + '_ {
        support::children::<ModuleDirective>(self.syntax())
    }

    pub fn directives(&self) -> impl Iterator<Item = SyntaxNode> + '_ {
        self.syntax().children().filter_map(|n| {
            if n.kind() == SyntaxKind::ModuleDirective {
                return n.children().find(|child| {
                    matches!(
                        child.kind(),
                        SyntaxKind::RequiresDirective
                            | SyntaxKind::ExportsDirective
                            | SyntaxKind::OpensDirective
                            | SyntaxKind::UsesDirective
                            | SyntaxKind::ProvidesDirective
                    )
                });
            }

            match n.kind() {
                SyntaxKind::RequiresDirective
                | SyntaxKind::ExportsDirective
                | SyntaxKind::OpensDirective
                | SyntaxKind::UsesDirective
                | SyntaxKind::ProvidesDirective => Some(n),
                _ => None,
            }
        })
    }
}

impl ModuleDirective {
    pub fn requires(&self) -> Option<RequiresDirective> {
        support::child::<RequiresDirective>(self.syntax())
    }

    pub fn exports(&self) -> Option<ExportsDirective> {
        support::child::<ExportsDirective>(self.syntax())
    }

    pub fn opens(&self) -> Option<OpensDirective> {
        support::child::<OpensDirective>(self.syntax())
    }

    pub fn uses(&self) -> Option<UsesDirective> {
        support::child::<UsesDirective>(self.syntax())
    }

    pub fn provides(&self) -> Option<ProvidesDirective> {
        support::child::<ProvidesDirective>(self.syntax())
    }
}

impl RequiresDirective {
    pub fn module(&self) -> Option<Name> {
        support::child::<Name>(self.syntax())
    }

    pub fn is_transitive(&self) -> bool {
        support::token(self.syntax(), SyntaxKind::TransitiveKw).is_some()
    }

    pub fn is_static(&self) -> bool {
        support::token(self.syntax(), SyntaxKind::StaticKw).is_some()
    }
}

impl ExportsDirective {
    pub fn package(&self) -> Option<Name> {
        support::child::<Name>(self.syntax())
    }

    pub fn to_modules(&self) -> impl Iterator<Item = Name> + '_ {
        support::children::<Name>(self.syntax()).skip(1)
    }
}

impl OpensDirective {
    pub fn package(&self) -> Option<Name> {
        support::child::<Name>(self.syntax())
    }

    pub fn to_modules(&self) -> impl Iterator<Item = Name> + '_ {
        support::children::<Name>(self.syntax()).skip(1)
    }
}

impl UsesDirective {
    pub fn service(&self) -> Option<Name> {
        support::child::<Name>(self.syntax())
    }
}

impl ProvidesDirective {
    pub fn service(&self) -> Option<Name> {
        support::child::<Name>(self.syntax())
    }

    pub fn implementations(&self) -> impl Iterator<Item = Name> + '_ {
        support::children::<Name>(self.syntax()).skip(1)
    }
}
