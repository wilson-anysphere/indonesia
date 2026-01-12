use crate::comment_printer::{fmt_comment, FmtCtx};
use crate::doc::Doc;
use crate::{Comment, CommentKind, TokenKey};
use nova_core::{TextRange, TextSize};
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
                self.print_verbatim_node_with_boundary_comments(decl.syntax())
            }
            other => self.print_verbatim_node_with_boundary_comments(other.syntax()),
        }
    }

    fn print_declaration_with_body(
        &mut self,
        decl: &SyntaxNode,
        body: Option<SyntaxNode>,
    ) -> Doc<'a> {
        let Some(body) = body else {
            return self.print_verbatim_node_with_boundary_comments(decl);
        };

        let Some((l_brace, r_brace)) = find_braces(&body) else {
            return self.print_verbatim_node_with_boundary_comments(decl);
        };

        let last_sig = decl
            .descendants_with_tokens()
            .filter_map(|el| el.into_token())
            .filter(|tok| {
                tok.kind() != SyntaxKind::Eof
                    && !tok.kind().is_trivia()
                    && !is_synthetic_missing(tok.kind())
            })
            .last();

        let header_end = u32::from(l_brace.text_range().start());
        let header_tokens: Vec<_> = super::significant_tokens(decl)
            .into_iter()
            .filter(|tok| u32::from(tok.text_range().start()) < header_end)
            .collect();
        if header_tokens.is_empty() {
            return self.print_verbatim_node_with_boundary_comments(decl);
        }
        let header = self.print_spaced_tokens(&header_tokens, 0);

        let body_doc = self.print_brace_body(&body, &l_brace, &r_brace);
        let trailer_start = u32::from(r_brace.text_range().end());
        let trailer_end = u32::from(decl.text_range().end());
        let trailer = if trailer_start < trailer_end {
            fallback::byte_range(self.source, trailer_start, trailer_end)
        } else {
            Doc::nil()
        };

        let trailing = last_sig
            .as_ref()
            .map(|tok| self.comments.take_trailing_doc(TokenKey::from(tok), 0))
            .unwrap_or_else(Doc::nil);

        Doc::concat([header, print::space(), body_doc, trailer, trailing])
    }

    fn print_brace_body(
        &mut self,
        body: &SyntaxNode,
        l_brace: &nova_syntax::SyntaxToken,
        r_brace: &nova_syntax::SyntaxToken,
    ) -> Doc<'a> {
        let inner_start = u32::from(l_brace.text_range().end());
        let inner_end = u32::from(r_brace.text_range().start());

        struct MemberLayout<'a> {
            members: Vec<SyntaxNode>,
            doc: Doc<'a>,
        }

        let member_layout: Option<MemberLayout<'a>> = match body.kind() {
            SyntaxKind::ClassBody => ast::ClassBody::cast(body.clone())
                .map(|b| b.members().map(|m| m.syntax().clone()).collect::<Vec<_>>())
                .and_then(|members| {
                    let doc = self.print_member_list(&members)?;
                    Some(MemberLayout { members, doc })
                }),
            SyntaxKind::InterfaceBody => ast::InterfaceBody::cast(body.clone())
                .map(|b| b.members().map(|m| m.syntax().clone()).collect::<Vec<_>>())
                .and_then(|members| {
                    let doc = self.print_member_list(&members)?;
                    Some(MemberLayout { members, doc })
                }),
            SyntaxKind::RecordBody => ast::RecordBody::cast(body.clone())
                .map(|b| b.members().map(|m| m.syntax().clone()).collect::<Vec<_>>())
                .and_then(|members| {
                    let doc = self.print_member_list(&members)?;
                    Some(MemberLayout { members, doc })
                }),
            SyntaxKind::AnnotationBody => ast::AnnotationBody::cast(body.clone())
                .map(|b| b.members().map(|m| m.syntax().clone()).collect::<Vec<_>>())
                .and_then(|members| {
                    let doc = self.print_member_list(&members)?;
                    Some(MemberLayout { members, doc })
                }),
            _ => None,
        };

        let leading_for_l_brace = self.comments.take_leading_doc(TokenKey::from(l_brace), 0);
        let open = Doc::concat([leading_for_l_brace, Doc::text("{")]);

        let inner_doc = if let Some(layout) = member_layout {
            let last_end = u32::from(layout.members[layout.members.len() - 1].text_range().end());

            let mut parts: Vec<Doc<'a>> = Vec::new();

            // Inline comments immediately after `{` (e.g. `{/* c */int x;}`) are stored as trailing
            // comments of the `{` token. When formatting a member list we want those comments to be
            // emitted at the start of the first member line instead (and ensure block comments are
            // followed by a space).
            let leading_from_l_brace = self
                .comments
                .take_trailing_as_leading_doc(TokenKey::from(l_brace), 0);
            if !leading_from_l_brace.is_nil() {
                parts.push(leading_from_l_brace);
            }

            parts.push(layout.doc);

            // Anything between the last member and the closing `}` that starts on a new line (e.g.
            // comments before the closing brace) is not associated with a specific member node, so
            // print it verbatim to avoid losing it. Skip content on the same line as the final
            // member because trailing comments for that line are already emitted via `CommentStore`
            // when formatting the member itself.
            if last_end < inner_end {
                if let Some(suffix_start) = find_first_line_break(self.source, last_end, inner_end)
                {
                    if let Some(suffix) =
                        self.print_verbatim_tokens(body, suffix_start, inner_end, true)
                    {
                        self.comments.consume_in_range(TextRange::new(
                            TextSize::from(suffix_start),
                            TextSize::from(inner_end),
                        ));
                        parts.push(suffix);
                    }
                }
            }

            Doc::concat(parts)
        } else if let Some(inner) = self.print_verbatim_tokens(body, inner_start, inner_end, false)
        {
            self.comments.consume_in_range(TextRange::new(
                TextSize::from(inner_start),
                TextSize::from(inner_end),
            ));
            inner
        } else {
            return Doc::concat([open, Doc::hardline(), Doc::text("}")]);
        };

        Doc::concat([
            open,
            Doc::concat([Doc::hardline(), inner_doc]).indent(),
            Doc::hardline(),
            Doc::text("}"),
        ])
    }

    fn print_member_list(&mut self, members: &[SyntaxNode]) -> Option<Doc<'a>> {
        if members.is_empty() {
            return None;
        }

        let mut parts: Vec<Doc<'a>> = Vec::new();
        let mut prev_end: Option<u32> = None;

        for (idx, member) in members.iter().enumerate() {
            if idx > 0 {
                let mut hardlines: usize = 1;
                if let Some(prev_end) = prev_end {
                    let start = u32::from(member.text_range().start());
                    if super::has_blank_line_between_offsets(self.source, prev_end, start) {
                        hardlines = 2;
                    }
                }

                if hardlines >= 2 {
                    if let Some((first, _)) = super::boundary_significant_tokens(member) {
                        let key = TokenKey::from(&first);
                        if self.comments.leading_blank_line_before(key) {
                            hardlines = hardlines.saturating_sub(1);
                        }
                    }
                }

                for _ in 0..hardlines {
                    parts.push(Doc::hardline());
                }
            }

            parts.push(self.print_member(member));
            prev_end = Some(u32::from(member.text_range().end()));
        }

        Some(Doc::concat(parts))
    }

    fn print_member(&mut self, node: &SyntaxNode) -> Doc<'a> {
        let Some((first, last)) = super::boundary_significant_tokens(node) else {
            self.comments.consume_in_range(node.text_range());
            return fallback::node(self.source, node);
        };

        let leading = self.comments.take_leading_doc(TokenKey::from(&first), 0);

        let start = u32::from(first.text_range().start());
        let end = u32::from(node.text_range().end());
        let content = self
            .print_verbatim_tokens(node, start, end, false)
            .unwrap_or_else(|| fallback::node(self.source, node));

        // Any comments printed verbatim from token slices must be marked consumed to avoid the
        // comment drain assertion.
        self.comments.consume_in_range(node.text_range());

        let trailing = self.comments.take_trailing_doc(TokenKey::from(&last), 0);
        Doc::concat([leading, content, trailing])
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
        let mut consumes_next_line_break = false;
        let mut at_line_start = true;
        let ctx = FmtCtx::new(0);

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
                    let mut breaks = crate::comment_printer::count_line_breaks(tok.text());
                    if consumes_next_line_break {
                        consumes_next_line_break = false;
                        if breaks == 0 {
                            continue;
                        }
                        breaks = breaks.saturating_sub(1);
                        if breaks == 0 {
                            continue;
                        }
                    }
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
                SyntaxKind::LineComment | SyntaxKind::BlockComment | SyntaxKind::DocComment => {
                    let had_ws = pending_ws.is_some();
                    if let Some(ws) = pending_ws.take() {
                        ws.flush(&mut parts);
                        at_line_start = matches!(ws, PendingWs::Hardlines(_));
                    }
                    let kind = match tok.kind() {
                        SyntaxKind::LineComment => CommentKind::Line,
                        SyntaxKind::BlockComment => CommentKind::Block,
                        SyntaxKind::DocComment => CommentKind::Doc,
                        _ => unreachable!("unexpected comment token kind"),
                    };

                    if kind == CommentKind::Line {
                        // `fmt_comment` always terminates line comments with a hardline. When
                        // emitting verbatim tokens for brace bodies we want the caller to control
                        // the final newline (e.g. avoid an extra blank line before `}`), so we
                        // print the comment text directly and enqueue a pending newline.
                        if has_content && !had_ws && !at_line_start {
                            // Ensure we don't glue `//` to the previous token when the source
                            // omits whitespace (`int x;// comment`).
                            parts.push(Doc::text(" "));
                        }

                        let raw = self
                            .source
                            .get(tok_start as usize..tok_end as usize)
                            .unwrap_or("");
                        let text = raw.trim_end_matches(['\r', '\n']);
                        parts.push(Doc::text(text));
                        has_content = true;

                        let newline = PendingWs::Hardlines(1);
                        pending_ws =
                            Some(pending_ws.map_or(newline, |existing| existing.merge(newline)));
                        consumes_next_line_break = false;
                        at_line_start = false;
                        continue;
                    }

                    if kind == CommentKind::Block && has_content && !had_ws && !at_line_start {
                        // Ensure we don't glue `/* ... */` to the previous token when the source
                        // omits whitespace (`int x;/* comment */`).
                        parts.push(Doc::text(" "));
                    }

                    let comment = Comment {
                        kind,
                        text_range: tok_range,
                        is_inline_with_prev: false,
                        is_inline_with_next: false,
                        blank_line_before: false,
                        blank_line_after: false,
                        force_own_line_after: kind == CommentKind::Doc,
                    };

                    parts.push(fmt_comment(&ctx, &comment, self.source));
                    has_content = true;
                    consumes_next_line_break = kind == CommentKind::Doc;
                    at_line_start = consumes_next_line_break;
                    // Ensure block comments cannot glue to the following token when the source has
                    // no whitespace between them (e.g. `/* comment */int x;`).
                    //
                    // Note: doc comments already end with a hardline via `fmt_comment`.
                    if kind == CommentKind::Block {
                        let ws = PendingWs::Space;
                        pending_ws = Some(pending_ws.map_or(ws, |existing| existing.merge(ws)));
                    }
                }
                _ => {
                    consumes_next_line_break = false;
                    if let Some(ws) = pending_ws.take() {
                        ws.flush(&mut parts);
                    }

                    let token_doc = match tok.kind() {
                        SyntaxKind::BlockComment | SyntaxKind::DocComment => {
                            let text = self
                                .source
                                .get(tok_start as usize..tok_end as usize)
                                .unwrap_or("");
                            if crate::comment_printer::comment_contains_line_break(text) {
                                fmt_multiline_comment(text, tok.kind())
                            } else {
                                fallback::byte_range(self.source, tok_start, tok_end)
                            }
                        }
                        _ => fallback::byte_range(self.source, tok_start, tok_end),
                    };
                    parts.push(token_doc);
                    has_content = true;
                    at_line_start = false;
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

fn find_first_line_break(source: &str, start: u32, end: u32) -> Option<u32> {
    let len = source.len();
    let mut start = start as usize;
    let mut end = end as usize;

    start = start.min(len);
    end = end.min(len);
    if start > end {
        std::mem::swap(&mut start, &mut end);
    }

    let bytes = source.as_bytes();
    let mut i = start;
    while i < end {
        let mut pos = match bytes[i] {
            b'\n' => {
                // If we started scanning at `\n` inside a CRLF sequence, prefer the `\r` byte so we
                // start at the beginning of the line-break token.
                if i > start && bytes[i - 1] == b'\r' {
                    i - 1
                } else {
                    i
                }
            }
            b'\r' => i,
            _ => {
                i += 1;
                continue;
            }
        };

        // `print_verbatim_tokens` only processes tokens whose `text_range` is fully within the
        // `[start, end]` bounds. The Java lexer typically groups trailing spaces and the following
        // line break into a single whitespace token, so starting *inside* that token would skip the
        // newline entirely. Back up over trailing indentation so we start at the whitespace token
        // boundary.
        while pos > start && matches!(bytes[pos - 1], b' ' | b'\t') {
            pos -= 1;
        }

        return Some(pos as u32);
    }

    None
}

fn fmt_multiline_comment<'a>(text: &'a str, kind: SyntaxKind) -> Doc<'a> {
    match kind {
        SyntaxKind::DocComment => fmt_multiline_doc_comment(text),
        _ => fmt_multiline_block_comment(text),
    }
}

fn fmt_multiline_block_comment<'a>(text: &'a str) -> Doc<'a> {
    let lines = crate::comment_printer::split_lines(text);
    if lines.is_empty() {
        return Doc::text(text);
    }

    let common = crate::comment_printer::common_indent(lines.iter().skip(1).map(|l| l.text));

    let mut parts: Vec<Doc<'a>> = Vec::with_capacity(lines.len() * 2);
    parts.push(Doc::text(lines[0].text));

    for idx in 1..lines.len() {
        if lines[idx - 1].has_line_break {
            parts.push(Doc::hardline());
            let line = crate::comment_printer::trim_indent(lines[idx].text, common);
            parts.push(Doc::text(line));
        } else {
            parts.push(Doc::text(lines[idx].text));
        }
    }

    Doc::concat(parts)
}

fn fmt_multiline_doc_comment<'a>(text: &'a str) -> Doc<'a> {
    let lines = crate::comment_printer::split_lines(text);
    if lines.is_empty() {
        return Doc::text(text);
    }

    let common = crate::comment_printer::common_indent(lines.iter().skip(1).map(|l| l.text));

    let mut parts: Vec<Doc<'a>> = Vec::with_capacity(lines.len() * 3);
    parts.push(Doc::text(lines[0].text));

    for idx in 1..lines.len() {
        if !lines[idx - 1].has_line_break {
            parts.push(Doc::text(lines[idx].text));
            continue;
        }

        parts.push(Doc::hardline());

        let raw = crate::comment_printer::trim_indent(lines[idx].text, common);
        if raw.trim().is_empty() {
            continue;
        }

        let trimmed = raw.trim_start_matches([' ', '\t']);
        if trimmed.starts_with("*/") {
            parts.push(Doc::text(trimmed));
        } else if let Some(rest) = trimmed.strip_prefix('*') {
            parts.push(Doc::concat([Doc::text(" *"), Doc::text(rest)]));
        } else {
            parts.push(Doc::text(raw));
        }
    }

    Doc::concat(parts)
}
