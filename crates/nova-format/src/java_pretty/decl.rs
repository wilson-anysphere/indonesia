use crate::doc::Doc;
use crate::TokenKey;
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
                self.comments.consume_in_range(decl.syntax().text_range());
                fallback::node(self.source, decl.syntax())
            }
            other => {
                self.comments.consume_in_range(other.syntax().text_range());
                fallback::node(self.source, other.syntax())
            }
        }
    }

    fn print_declaration_with_body(
        &mut self,
        decl: &SyntaxNode,
        body: Option<SyntaxNode>,
    ) -> Doc<'a> {
        let Some(body) = body else {
            self.comments.consume_in_range(decl.text_range());
            return fallback::node(self.source, decl);
        };

        let Some((l_brace, r_brace)) = find_braces(&body) else {
            self.comments.consume_in_range(decl.text_range());
            return fallback::node(self.source, decl);
        };

        let first_sig = decl
            .descendants_with_tokens()
            .filter_map(|el| el.into_token())
            .find(|tok| tok.kind() != SyntaxKind::Eof && !tok.kind().is_trivia() && !is_synthetic_missing(tok.kind()));

        let header_start = first_sig
            .as_ref()
            .map(|tok| u32::from(tok.text_range().start()))
            .unwrap_or_else(|| u32::from(decl.text_range().start()));
        let header_end = u32::from(l_brace.text_range().start());
        let header = self
            .print_verbatim_tokens(decl, header_start, header_end, true)
            .unwrap_or_else(|| fallback::byte_range(self.source, header_start, header_end));

        let leading = first_sig
            .as_ref()
            .map(|tok| self.comments.take_leading_doc(TokenKey::from(tok), 0))
            .unwrap_or_else(Doc::nil);

        let body_doc = self.print_brace_body(&body, &l_brace, &r_brace);
        let trailer_start = u32::from(r_brace.text_range().end());
        let trailer_end = u32::from(decl.text_range().end());
        let trailer = if trailer_start < trailer_end {
            fallback::byte_range(self.source, trailer_start, trailer_end)
        } else {
            Doc::nil()
        };

        // Anything we still print via `fallback::byte_range`/raw slices has already emitted comment
        // tokens, so mark them as consumed so we can debug-assert that nothing is silently dropped.
        self.comments.consume_in_range(decl.text_range());

        Doc::concat([leading, header, print::space(), body_doc, trailer])
    }

    fn print_brace_body(
        &mut self,
        body: &SyntaxNode,
        l_brace: &nova_syntax::SyntaxToken,
        r_brace: &nova_syntax::SyntaxToken,
    ) -> Doc<'a> {
        let inner_start = u32::from(l_brace.text_range().end());
        let inner_end = u32::from(r_brace.text_range().start());
        let inner = self.print_verbatim_tokens(body, inner_start, inner_end, false);
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
        preserve_leading_line_breaks: bool,
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
                    let breaks = crate::comment_printer::count_line_breaks(tok.text());
                    if !has_content {
                        if preserve_leading_line_breaks && breaks > 0 {
                            let new_ws = PendingWs::Hardlines(breaks.min(2) as usize);
                            pending_ws = Some(pending_ws.map_or(new_ws, |ws| ws.merge(new_ws)));
                        }
                        continue;
                    }

                    let new_ws = if breaks == 0 {
                        PendingWs::Space
                    } else {
                        PendingWs::Hardlines(breaks.min(2) as usize)
                    };
                    pending_ws = Some(pending_ws.map_or(new_ws, |ws| ws.merge(new_ws)));
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
    fn merge(self, other: PendingWs) -> PendingWs {
        match (self, other) {
            (Self::Hardlines(a), Self::Hardlines(b)) => Self::Hardlines(a.max(b)),
            (Self::Hardlines(a), Self::Space) => Self::Hardlines(a),
            (Self::Space, Self::Hardlines(b)) => Self::Hardlines(b),
            (Self::Space, Self::Space) => Self::Space,
        }
    }

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
