use crate::doc::Doc;
use nova_syntax::{ast, AstNode, SyntaxKind, SyntaxNode};

use super::{fallback, print, JavaPrettyFormatter};

impl<'a> JavaPrettyFormatter<'a> {
    pub(super) fn print_type_declaration(&mut self, decl: ast::TypeDeclaration) -> Doc<'a> {
        match decl {
            ast::TypeDeclaration::ClassDeclaration(decl) => self.print_declaration_with_body(
                decl.syntax(),
                decl.body().map(|b| b.syntax().clone()),
            ),
            ast::TypeDeclaration::InterfaceDeclaration(decl) => self.print_declaration_with_body(
                decl.syntax(),
                decl.body().map(|b| b.syntax().clone()),
            ),
            ast::TypeDeclaration::EnumDeclaration(decl) => self.print_declaration_with_body(
                decl.syntax(),
                decl.body().map(|b| b.syntax().clone()),
            ),
            ast::TypeDeclaration::RecordDeclaration(decl) => self.print_declaration_with_body(
                decl.syntax(),
                decl.body().map(|b| b.syntax().clone()),
            ),
            ast::TypeDeclaration::AnnotationTypeDeclaration(decl) => self
                .print_declaration_with_body(
                    decl.syntax(),
                    decl.body().map(|b| b.syntax().clone()),
                ),
            ast::TypeDeclaration::EmptyDeclaration(decl) => {
                fallback::node(self.source, decl.syntax())
            }
            other => fallback::node(self.source, other.syntax()),
        }
    }

    fn print_declaration_with_body(
        &mut self,
        decl: &SyntaxNode,
        body: Option<SyntaxNode>,
    ) -> Doc<'a> {
        let Some(body) = body else {
            return fallback::node(self.source, decl);
        };

        let Some((l_brace, r_brace)) = find_braces(&body) else {
            return fallback::node(self.source, decl);
        };

        let header_start = u32::from(decl.text_range().start());
        let header_end = u32::from(l_brace.text_range().start());
        // For the header we want to avoid trailing whitespace before `{` so the formatter doesn't
        // preserve awkward `class Foo  {` spacing.
        let header = header_text_trimmed(self.source, header_start, header_end).map_or_else(
            || fallback::byte_range(self.source, header_start, header_end),
            Doc::text,
        );

        let body_doc = self.print_brace_body(&l_brace, &r_brace);

        Doc::concat([header, print::space(), body_doc])
    }

    fn print_brace_body(
        &mut self,
        l_brace: &nova_syntax::SyntaxToken,
        r_brace: &nova_syntax::SyntaxToken,
    ) -> Doc<'a> {
        let inner_start = u32::from(l_brace.text_range().end()) as usize;
        let inner_end = u32::from(r_brace.text_range().start()) as usize;
        let inner_start = inner_start.min(self.source.len());
        let inner_end = inner_end.min(self.source.len());
        let (inner_start, inner_end) = if inner_start <= inner_end {
            (inner_start, inner_end)
        } else {
            (inner_end, inner_start)
        };
        let inner = self.source.get(inner_start..inner_end).unwrap_or("");
        let inner = inner.trim_matches(|c: char| matches!(c, ' ' | '\t' | '\n' | '\r'));

        if inner.is_empty() {
            return Doc::concat([Doc::text("{"), Doc::hardline(), Doc::text("}")]);
        }

        let inner_doc = Doc::text(inner);
        Doc::concat([
            Doc::text("{"),
            Doc::concat([Doc::hardline(), inner_doc]).indent(),
            Doc::hardline(),
            Doc::text("}"),
        ])
    }
}

fn header_text_trimmed<'a>(source: &'a str, start: u32, end: u32) -> Option<&'a str> {
    let start = start as usize;
    let end = end as usize;
    let start = start.min(source.len());
    let end = end.min(source.len());
    let slice = source.get(start..end)?;
    Some(slice.trim_end_matches(|c: char| matches!(c, ' ' | '\t' | '\n' | '\r')))
}

fn find_braces(body: &SyntaxNode) -> Option<(nova_syntax::SyntaxToken, nova_syntax::SyntaxToken)> {
    let mut l_brace = None;
    let mut r_brace = None;
    for el in body.children_with_tokens() {
        let Some(tok) = el.into_token() else {
            continue;
        };
        if is_synthetic_missing(tok.kind()) {
            continue;
        }
        match tok.kind() {
            SyntaxKind::LBrace if l_brace.is_none() => l_brace = Some(tok),
            SyntaxKind::RBrace => r_brace = Some(tok),
            _ => {}
        }
    }

    match (l_brace, r_brace) {
        (Some(l), Some(r)) => Some((l, r)),
        _ => None,
    }
}

fn is_synthetic_missing(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::MissingSemicolon
            | SyntaxKind::MissingRParen
            | SyntaxKind::MissingRBrace
            | SyntaxKind::MissingRBracket
            | SyntaxKind::MissingGreater
    )
}
