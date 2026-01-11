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

        let body_doc = self.print_brace_body(&body, &l_brace, &r_brace);

        Doc::concat([header, print::space(), body_doc])
    }

    fn print_brace_body(
        &mut self,
        body: &SyntaxNode,
        l_brace: &nova_syntax::SyntaxToken,
        r_brace: &nova_syntax::SyntaxToken,
    ) -> Doc<'a> {
        let inner_start = u32::from(l_brace.text_range().end());
        let inner_end = u32::from(r_brace.text_range().start());
        let inner = self.print_verbatim_tokens(body, inner_start, inner_end);
        if inner.is_none() {
            return Doc::concat([Doc::text("{"), Doc::hardline(), Doc::text("}")]);
        }

        let inner_doc = inner.unwrap();
        Doc::concat([
            Doc::text("{"),
            Doc::concat([Doc::hardline(), inner_doc]).indent(),
            Doc::hardline(),
            Doc::text("}"),
        ])
    }

    fn print_verbatim_tokens(
        &self,
        node: &SyntaxNode,
        start: u32,
        end: u32,
    ) -> Option<Doc<'a>> {
        if start >= end {
            return None;
        }

        let mut parts: Vec<Doc<'a>> = Vec::new();
        let mut has_content = false;
        let mut pending_ws: Option<PendingWs> = None;

        for el in node.descendants_with_tokens() {
            let Some(tok) = el.into_token() else {
                continue;
            };
            if is_synthetic_missing(tok.kind()) || tok.kind() == SyntaxKind::Eof {
                continue;
            }

            let tok_range = tok.text_range();
            let tok_start = u32::from(tok_range.start());
            let tok_end = u32::from(tok_range.end());
            if tok_start < start || tok_end > end {
                continue;
            }

            match tok.kind() {
                SyntaxKind::Whitespace => {
                    if !has_content {
                        continue;
                    }

                    let breaks = crate::comment_printer::count_line_breaks(tok.text());
                    pending_ws = Some(if breaks == 0 {
                        PendingWs::Space
                    } else {
                        PendingWs::Hardlines(breaks.min(2) as usize)
                    });
                }
                _ => {
                    if let Some(ws) = pending_ws.take() {
                        ws.flush(&mut parts);
                    }
                    parts.push(fallback::byte_range(self.source, tok_start, tok_end));
                    has_content = true;
                }
            }
        }

        if !has_content {
            return None;
        }

        if parts.is_empty() {
            None
        } else {
            Some(Doc::concat(parts))
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum PendingWs {
    Space,
    Hardlines(usize),
}

impl PendingWs {
    fn flush<'a>(self, out: &mut Vec<Doc<'a>>) {
        match self {
            Self::Space => out.push(Doc::text(" ")),
            Self::Hardlines(count) => {
                for _ in 0..count {
                    out.push(Doc::hardline());
                }
            }
        }
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
