use super::support;
use crate::ast::AstNode;
use crate::{JavaLanguage, SyntaxKind, SyntaxNode, SyntaxToken};

impl super::ImportDeclaration {
    pub fn is_static(&self) -> bool {
        support::token(self.syntax(), SyntaxKind::StaticKw).is_some()
    }

    pub fn is_wildcard(&self) -> bool {
        support::token(self.syntax(), SyntaxKind::Star).is_some()
            || self
                .name()
                .and_then(|name| support::token(name.syntax(), SyntaxKind::Star))
                .is_some()
    }
}

impl super::Modifiers {
    pub fn keywords(&self) -> impl Iterator<Item = SyntaxToken> + '_ {
        self.syntax()
            .children_with_tokens()
            .filter_map(|it| it.into_token())
            .filter(|t| !t.kind().is_trivia())
    }
}

impl super::Name {
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

impl super::MethodDeclaration {
    pub fn parameters(&self) -> impl Iterator<Item = super::Parameter> + '_ {
        // `flat_map(|list| list.parameters())` does not compile because the iterator borrows
        // the moved `list`. Collect into a small buffer instead.
        self.parameter_list()
            .into_iter()
            .flat_map(|list| list.parameters().collect::<Vec<_>>())
    }

    pub fn return_type(&self) -> Option<super::Type> {
        // Avoid mis-classifying thrown types as the return type for `void` methods.
        if support::token(self.syntax(), SyntaxKind::VoidKw).is_some() {
            return None;
        }
        support::child::<super::Type>(self.syntax())
    }
}

impl super::FieldDeclaration {
    pub fn declarators(&self) -> impl Iterator<Item = super::VariableDeclarator> + '_ {
        self.declarator_list()
            .into_iter()
            .flat_map(|list| list.declarators().collect::<Vec<_>>())
    }
}

impl super::SwitchStatement {
    pub fn labels(&self) -> impl Iterator<Item = super::SwitchLabel> + '_ {
        let mut out = Vec::new();
        let Some(block) = self.block() else {
            return out.into_iter();
        };

        // `parse_switch_block` currently wraps labels inside `SwitchGroup`/`SwitchRule` nodes, so
        // `SwitchLabel` is not a direct child of the `SwitchBlock`.
        for child in block.syntax().children() {
            match child.kind() {
                SyntaxKind::SwitchGroup | SyntaxKind::SwitchRule => {
                    out.extend(child.children().filter_map(super::SwitchLabel::cast));
                }
                SyntaxKind::SwitchLabel => {
                    if let Some(label) = super::SwitchLabel::cast(child) {
                        out.push(label);
                    }
                }
                _ => {}
            }
        }

        out.into_iter()
    }
}

impl super::SwitchLabel {
    pub fn is_case(&self) -> bool {
        support::token(self.syntax(), SyntaxKind::CaseKw).is_some()
    }

    pub fn is_default(&self) -> bool {
        support::token(self.syntax(), SyntaxKind::DefaultKw).is_some()
    }

    pub fn has_arrow(&self) -> bool {
        support::token(self.syntax(), SyntaxKind::Arrow).is_some()
    }

    pub fn expressions(&self) -> impl Iterator<Item = super::Expression> + '_ {
        self.elements().filter_map(|element| element.expression())
    }
}

impl super::CaseLabelElement {
    pub fn is_default(&self) -> bool {
        support::token(self.syntax(), SyntaxKind::DefaultKw).is_some()
    }
}

impl super::ModuleDeclaration {
    pub fn is_open(&self) -> bool {
        support::token(self.syntax(), SyntaxKind::OpenKw).is_some()
    }

    pub fn directives(&self) -> impl Iterator<Item = super::ModuleDirectiveKind> + '_ {
        self.body()
            .into_iter()
            .flat_map(|body| body.directive_wrappers().collect::<Vec<_>>())
            .filter_map(|wrapper| wrapper.directive())
    }
}

impl super::ModuleBody {
    pub fn directive_items(&self) -> impl Iterator<Item = super::ModuleDirective> + '_ {
        self.directive_wrappers()
    }

    /// Returns the raw directive syntax nodes (unwrapping `ModuleDirective` wrapper nodes).
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

impl super::RequiresDirective {
    pub fn is_transitive(&self) -> bool {
        support::token(self.syntax(), SyntaxKind::TransitiveKw).is_some()
    }

    pub fn is_static(&self) -> bool {
        support::token(self.syntax(), SyntaxKind::StaticKw).is_some()
    }
}

struct NamesAfterKeyword {
    iter: rowan::SyntaxElementChildren<JavaLanguage>,
    keyword: SyntaxKind,
    started: bool,
}

impl NamesAfterKeyword {
    fn new(parent: &SyntaxNode, keyword: SyntaxKind) -> Self {
        Self {
            iter: parent.children_with_tokens(),
            keyword,
            started: false,
        }
    }
}

impl Iterator for NamesAfterKeyword {
    type Item = super::Name;

    fn next(&mut self) -> Option<Self::Item> {
        use rowan::NodeOrToken;

        while let Some(el) = self.iter.next() {
            match el {
                NodeOrToken::Token(tok) if tok.kind() == self.keyword => {
                    self.started = true;
                }
                NodeOrToken::Node(node) if self.started => {
                    if let Some(name) = super::Name::cast(node) {
                        return Some(name);
                    }
                }
                _ => {}
            }
        }
        None
    }
}

impl super::ExportsDirective {
    pub fn to_modules(&self) -> impl Iterator<Item = super::Name> + '_ {
        NamesAfterKeyword::new(self.syntax(), SyntaxKind::ToKw)
    }
}

impl super::OpensDirective {
    pub fn to_modules(&self) -> impl Iterator<Item = super::Name> + '_ {
        NamesAfterKeyword::new(self.syntax(), SyntaxKind::ToKw)
    }
}

impl super::ProvidesDirective {
    pub fn implementations(&self) -> impl Iterator<Item = super::Name> + '_ {
        NamesAfterKeyword::new(self.syntax(), SyntaxKind::WithKw)
    }
}
