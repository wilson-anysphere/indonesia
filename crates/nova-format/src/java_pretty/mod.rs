use crate::doc::{self, Doc, PrintConfig};
use crate::{
    ends_with_line_break, split_lines_inclusive, FormatConfig, IndentStyle, JavaComments,
    NewlineStyle, TokenKey,
};
use nova_syntax::{ast, lex, AstNode, JavaParseResult, SyntaxKind, SyntaxNode, SyntaxToken};

mod decl;
mod expr;
mod fallback;
mod print;
mod stmt;

pub(crate) struct JavaPrettyFormatter<'a> {
    pub(crate) parse: &'a JavaParseResult,
    pub(crate) source: &'a str,
    pub(crate) config: &'a FormatConfig,
    pub(crate) newline: NewlineStyle,
    pub(crate) comments: JavaComments<'a>,
}

impl<'a> JavaPrettyFormatter<'a> {
    pub(crate) fn new(
        parse: &'a JavaParseResult,
        source: &'a str,
        config: &'a FormatConfig,
        newline: NewlineStyle,
    ) -> Self {
        let comments = JavaComments::new(&parse.syntax(), source);
        Self {
            parse,
            source,
            config,
            newline,
            comments,
        }
    }

    pub(crate) fn build_doc(&mut self) -> Doc<'a> {
        let root = self.parse.syntax();
        match ast::CompilationUnit::cast(root.clone()) {
            Some(unit) => self.print_compilation_unit(unit.syntax()),
            None => {
                self.comments.consume_in_range(root.text_range());
                fallback::node(self.source, &root)
            }
        }
    }

    pub(crate) fn format(mut self, input_has_final_newline: bool) -> String {
        let doc = self.build_doc();
        let mut out = doc::print(
            doc,
            PrintConfig {
                max_width: self.config.max_line_length,
                indent_width: self.config.indent_width,
                newline: self.newline.as_str(),
            },
        );
        finalize_output(&mut out, self.config, input_has_final_newline, self.newline);
        if self.config.indent_style == IndentStyle::Tabs {
            out = tabs_for_indentation(&out, self.config.indent_width);
        }
        out
    }

    fn print_compilation_unit(&mut self, node: &SyntaxNode) -> Doc<'a> {
        let children: Vec<SyntaxNode> = node.children().collect();

        let mut parts: Vec<Doc<'a>> = Vec::new();
        let mut pending_hardlines: usize = 0;
        // Track the last significant token of the previously printed top-level item so we can
        // preserve a single blank line between consecutive type declarations when the original
        // source gap is whitespace-only (comments are handled by `JavaComments`).
        let mut prev_item_last_sig_end: Option<u32> = None;
        let mut prev_item_was_type_decl: bool = false;

        let mut idx = 0usize;
        while idx < children.len() {
            let child = &children[idx];
            match child.kind() {
                SyntaxKind::PackageDeclaration => {
                    self.flush_pending_hardlines(&mut parts, &mut pending_hardlines, Some(child));
                    parts.push(self.print_package_declaration(child));
                    // Exactly one blank line after a package declaration.
                    pending_hardlines = 2;
                    prev_item_last_sig_end = boundary_significant_tokens(child)
                        .map(|(_, last)| u32::from(last.text_range().end()));
                    prev_item_was_type_decl = false;
                    idx += 1;
                }
                SyntaxKind::ImportDeclaration => {
                    let start = idx;
                    while idx < children.len()
                        && children[idx].kind() == SyntaxKind::ImportDeclaration
                    {
                        idx += 1;
                    }
                    self.flush_pending_hardlines(
                        &mut parts,
                        &mut pending_hardlines,
                        children.get(start),
                    );
                    parts.push(self.print_import_block(&children[start..idx]));
                    if idx < children.len() {
                        // Blank line between the imports section and the first declaration.
                        pending_hardlines = 2;
                    }
                    prev_item_last_sig_end = boundary_significant_tokens(&children[idx - 1])
                        .map(|(_, last)| u32::from(last.text_range().end()));
                    prev_item_was_type_decl = false;
                }
                SyntaxKind::ModuleDeclaration => {
                    self.flush_pending_hardlines(&mut parts, &mut pending_hardlines, Some(child));
                    parts.push(self.print_module_declaration(child));
                    pending_hardlines = 1;
                    prev_item_last_sig_end = boundary_significant_tokens(child)
                        .map(|(_, last)| u32::from(last.text_range().end()));
                    prev_item_was_type_decl = false;
                    idx += 1;
                }
                _ => {
                    let ty = ast::TypeDeclaration::cast(child.clone());
                    let is_type_decl = ty.is_some();
                    let bounds = boundary_significant_tokens(child);

                    // Preserve a single blank line (collapse >1) between consecutive type
                    // declarations when the source gap contains only whitespace. If the gap
                    // contains comments or other non-whitespace, leave spacing to `JavaComments`.
                    if prev_item_was_type_decl && is_type_decl {
                        if let (Some(prev_end), Some((first, _))) =
                            (prev_item_last_sig_end, bounds.as_ref())
                        {
                            let next_start = u32::from(first.text_range().start());
                            if has_whitespace_only_blank_line_between_offsets(
                                self.source,
                                prev_end,
                                next_start,
                            ) {
                                pending_hardlines = pending_hardlines.max(2);
                            }
                        }
                    }

                    self.flush_pending_hardlines(&mut parts, &mut pending_hardlines, Some(child));
                    if let Some(ty) = ty {
                        parts.push(self.print_type_declaration(ty));
                    } else {
                        // Fallback nodes print verbatim source, including any nested comment tokens.
                        // Consume those comments so they don't trip the drain assertion.
                        parts.push(self.print_verbatim_node_with_boundary_comments(child));
                    }
                    pending_hardlines = 1;
                    prev_item_last_sig_end =
                        bounds.map(|(_, last)| u32::from(last.text_range().end()));
                    prev_item_was_type_decl = is_type_decl;
                    idx += 1;
                }
            }
        }

        // Comments at EOF are anchored to the EOF token.
        let eof = self
            .parse
            .syntax()
            .descendants_with_tokens()
            .filter_map(|el| el.into_token())
            .find(|tok| tok.kind() == SyntaxKind::Eof);
        if let Some(eof) = eof {
            let eof_key = TokenKey::from(&eof);
            let blank_line_before = self.comments.leading_blank_line_before(eof_key);
            let trailing = self.comments.take_leading_doc(eof_key, 0);
            if !trailing.is_nil() {
                // EOF-anchored comments are not inline with the previous significant token, so they
                // must appear on a new line. Ensure we always flush at least one hardline before
                // printing them.
                if pending_hardlines == 0 {
                    pending_hardlines = 1;
                }

                // Avoid double-inserting blank lines when the pretty printer already emitted a
                // structural blank line and the comment metadata also indicates an extra blank
                // line before the comment block.
                if pending_hardlines >= 2 && blank_line_before {
                    pending_hardlines = pending_hardlines.saturating_sub(1);
                }
                self.flush_pending_hardlines(&mut parts, &mut pending_hardlines, None);
                parts.push(trailing);
            }
        }

        Doc::concat(parts)
    }

    fn flush_pending_hardlines(
        &mut self,
        out: &mut Vec<Doc<'a>>,
        pending: &mut usize,
        next: Option<&SyntaxNode>,
    ) {
        if out.is_empty() {
            *pending = 0;
            return;
        }

        let mut count = *pending;
        if count >= 2 {
            if let Some(next) = next {
                if let Some((first, _)) = boundary_significant_tokens(next) {
                    let key = TokenKey::from(&first);
                    if self.comments.leading_blank_line_before(key) {
                        count = count.saturating_sub(1);
                    }
                }
            }
        }

        for _ in 0..count {
            out.push(Doc::hardline());
        }
        *pending = 0;
    }

    fn print_package_declaration(&mut self, node: &SyntaxNode) -> Doc<'a> {
        let tokens = significant_tokens(node);
        if tokens.is_empty() {
            return self.print_verbatim_node_with_boundary_comments(node);
        }
        self.print_spaced_tokens(&tokens, 0)
    }

    fn print_import_block(&mut self, nodes: &[SyntaxNode]) -> Doc<'a> {
        let mut parts: Vec<Doc<'a>> = Vec::new();
        let mut last_static: Option<bool> = None;
        let mut prev_end: Option<u32> = None;

        for (idx, node) in nodes.iter().enumerate() {
            let is_static = import_is_static(node);
            if idx > 0 {
                let mut needs_blank_line = false;
                if let Some(prev_static) = last_static {
                    if prev_static != is_static {
                        needs_blank_line = true;
                    }
                }
                if let Some(prev_end) = prev_end {
                    let start = u32::from(node.text_range().start());
                    if has_blank_line_between_offsets(self.source, prev_end, start) {
                        needs_blank_line = true;
                    }
                }

                let mut hardlines: usize = if needs_blank_line { 2 } else { 1 };
                if hardlines >= 2 {
                    if let Some((first, _)) = boundary_significant_tokens(node) {
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

            parts.push(self.print_import_declaration(node));
            last_static = Some(is_static);
            prev_end = Some(u32::from(node.text_range().end()));
        }

        Doc::concat(parts)
    }

    fn print_import_declaration(&mut self, node: &SyntaxNode) -> Doc<'a> {
        let tokens = significant_tokens(node);
        if tokens.is_empty() {
            return self.print_verbatim_node_with_boundary_comments(node);
        }
        self.print_spaced_tokens(&tokens, 0)
    }

    fn print_module_declaration(&mut self, node: &SyntaxNode) -> Doc<'a> {
        let Some(module) = ast::ModuleDeclaration::cast(node.clone()) else {
            return self.print_verbatim_node_with_boundary_comments(node);
        };
        let Some(body) = module.body().map(|b| b.syntax().clone()) else {
            return self.print_verbatim_node_with_boundary_comments(node);
        };
        let Some((l_brace, r_brace)) = find_braces(&body) else {
            return self.print_verbatim_node_with_boundary_comments(node);
        };

        let header_end = u32::from(l_brace.text_range().start());
        let header_tokens: Vec<SyntaxToken> = significant_tokens(node)
            .into_iter()
            .filter(|tok| u32::from(tok.text_range().start()) < header_end)
            .collect();
        let header = self.print_spaced_tokens(&header_tokens, 0);
        let l_brace_doc = self.print_token_with_comments(&l_brace, 0);
        let r_brace_doc = self.print_token_with_comments(&r_brace, 0);

        let directives: Vec<SyntaxNode> = body
            .children()
            .filter(|n| n.kind() == SyntaxKind::ModuleDirective)
            .collect();
        let inner = self.print_module_directives(&directives);

        let body = match inner {
            Some(inner) => Doc::concat([Doc::hardline(), inner]).indent(),
            None => Doc::nil(),
        };

        Doc::concat([
            header,
            print::space(),
            l_brace_doc,
            body,
            Doc::hardline(),
            r_brace_doc,
        ])
    }

    fn print_module_directives(&mut self, directives: &[SyntaxNode]) -> Option<Doc<'a>> {
        if directives.is_empty() {
            return None;
        }

        let mut parts: Vec<Doc<'a>> = Vec::new();
        let mut prev_end: Option<u32> = None;

        for (idx, directive) in directives.iter().enumerate() {
            if idx > 0 {
                let mut hardlines: usize = 1;
                if let Some(prev_end) = prev_end {
                    let start = u32::from(directive.text_range().start());
                    if has_blank_line_between_offsets(self.source, prev_end, start) {
                        hardlines = 2;
                    }
                }

                if hardlines >= 2 {
                    if let Some((first, _)) = boundary_significant_tokens(directive) {
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

            let tokens = significant_tokens(directive);
            if tokens.is_empty() {
                parts.push(self.print_verbatim_node_with_boundary_comments(directive));
            } else {
                parts.push(self.print_spaced_tokens(&tokens, 0));
            }
            prev_end = Some(u32::from(directive.text_range().end()));
        }

        Some(Doc::concat(parts))
    }

    fn print_spaced_tokens(&mut self, tokens: &[SyntaxToken], indent: usize) -> Doc<'a> {
        let mut parts: Vec<Doc<'a>> = Vec::new();
        let mut last_sig: Option<LastSig<'a>> = None;

        for token in tokens {
            let key = TokenKey::from(token);
            let leading = self.comments.take_leading_doc(key, indent);
            if !leading.is_nil() {
                parts.push(leading);
                last_sig = None;
            }

            let text = token_text(self.source, token);
            if needs_space_between(last_sig.as_ref(), token.kind(), text) {
                parts.push(print::space());
            }
            parts.push(fallback::token(self.source, token));
            last_sig = Some(LastSig {
                kind: token.kind(),
                text,
            });

            let trailing = self.comments.take_trailing_doc(key, indent);
            if !trailing.is_nil() {
                parts.push(trailing);
            }
        }

        Doc::concat(parts)
    }

    fn print_token_with_comments(&mut self, token: &SyntaxToken, indent: usize) -> Doc<'a> {
        let key = TokenKey::from(token);
        let leading = self.comments.take_leading_doc(key, indent);
        let trailing = self.comments.take_trailing_doc(key, indent);
        Doc::concat([leading, fallback::token(self.source, token), trailing])
    }

    fn print_verbatim_node_with_boundary_comments(&mut self, node: &SyntaxNode) -> Doc<'a> {
        let Some((first, last)) = boundary_significant_tokens(node) else {
            self.comments.consume_in_range(node.text_range());
            return fallback::node(self.source, node);
        };

        // The verbatim fallback will include any comment tokens *inside* `node`, so consume them to
        // satisfy the drain assertion. Any comments anchored to boundary tokens but living outside
        // the node range (e.g. `import ...; // trailing`) must still be emitted explicitly.
        self.comments.consume_in_range(node.text_range());

        let leading = self.comments.take_leading_doc(TokenKey::from(&first), 0);
        let trailing = self.comments.take_trailing_doc(TokenKey::from(&last), 0);
        Doc::concat([leading, fallback::node(self.source, node), trailing])
    }
}

fn tabs_for_indentation(text: &str, indent_width: usize) -> String {
    if indent_width == 0 {
        return text.to_string();
    }

    // `IndentStyle::Tabs` is implemented as a post-processing step because the Doc printer only
    // knows how to emit indentation in spaces.
    //
    // This MUST be semantics-preserving: leading whitespace inside multi-line literals (notably
    // text blocks) is part of the program and must not be rewritten.
    //
    // We conservatively skip indentation conversion for any line whose start offset falls inside a
    // token that can legally contain newlines + leading whitespace as part of its payload.
    let protected = protected_indent_ranges(text);
    let mut protected_idx = 0usize;

    let mut out = String::with_capacity(text.len());
    let mut offset = 0usize;
    for line in split_lines_inclusive(text) {
        let (content, suffix) = strip_line_ending(line);

        let in_protected = is_offset_in_range_list(offset, &protected, &mut protected_idx);
        offset = offset.saturating_add(line.len());
        if in_protected {
            out.push_str(content);
            out.push_str(suffix);
            continue;
        }

        let space_count = content
            .as_bytes()
            .iter()
            .take_while(|b| **b == b' ')
            .count();
        let tabs = space_count / indent_width;
        let spaces = space_count % indent_width;

        for _ in 0..tabs {
            out.push('\t');
        }
        for _ in 0..spaces {
            out.push(' ');
        }
        out.push_str(&content[space_count..]);
        out.push_str(suffix);
    }

    out
}

fn protected_indent_ranges(text: &str) -> Vec<std::ops::Range<usize>> {
    lex(text)
        .into_iter()
        .filter(|tok| matches_protected_indent_token(tok.kind))
        .map(|tok| {
            let start = tok.range.start as usize;
            let end = tok.range.end as usize;
            start..end
        })
        .collect()
}

fn matches_protected_indent_token(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        // Multi-line literals: leading whitespace after embedded newlines is semantically
        // meaningful.
        SyntaxKind::TextBlock
            | SyntaxKind::StringTemplateText
            // Template delimiters are currently lexed as distinct tokens. Even when they don't
            // span multiple lines, treating them as protected is a safe fallback.
            | SyntaxKind::StringTemplateStart
            | SyntaxKind::StringTemplateExprStart
            | SyntaxKind::StringTemplateExprEnd
            | SyntaxKind::StringTemplateEnd
            // The lexer represents unterminated text blocks as `Error` tokens that may span
            // multiple lines; preserve verbatim.
            | SyntaxKind::Error
            // Java string/char literals cannot contain newlines in valid code, but malformed input
            // can still surface `Error` tokens and we want to be robust.
            | SyntaxKind::StringLiteral
            | SyntaxKind::CharLiteral
    )
}

fn is_offset_in_range_list(
    offset: usize,
    ranges: &[std::ops::Range<usize>],
    idx: &mut usize,
) -> bool {
    while *idx < ranges.len() && ranges[*idx].end <= offset {
        *idx += 1;
    }

    ranges
        .get(*idx)
        .is_some_and(|range| range.start < offset && offset < range.end)
}

fn strip_line_ending(line: &str) -> (&str, &str) {
    if let Some(prefix) = line.strip_suffix("\r\n") {
        (prefix, "\r\n")
    } else if let Some(prefix) = line.strip_suffix('\n') {
        (prefix, "\n")
    } else if let Some(prefix) = line.strip_suffix('\r') {
        (prefix, "\r")
    } else {
        (line, "")
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

fn boundary_significant_tokens(node: &SyntaxNode) -> Option<(SyntaxToken, SyntaxToken)> {
    let mut iter = node
        .descendants_with_tokens()
        .filter_map(|el| el.into_token())
        .filter(|tok| {
            tok.kind() != SyntaxKind::Eof
                && !tok.kind().is_trivia()
                && !is_synthetic_missing(tok.kind())
        });

    let first = iter.next()?;
    let mut last = first.clone();
    for tok in iter {
        last = tok;
    }
    Some((first, last))
}

fn significant_tokens(node: &SyntaxNode) -> Vec<SyntaxToken> {
    node.descendants_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|tok| tok.kind() != SyntaxKind::Eof)
        .filter(|tok| !tok.kind().is_trivia())
        .filter(|tok| !is_synthetic_missing(tok.kind()))
        .collect()
}

fn token_text<'a>(source: &'a str, token: &SyntaxToken) -> &'a str {
    let range = token.text_range();
    let start = u32::from(range.start()) as usize;
    let end = u32::from(range.end()) as usize;
    source.get(start..end).unwrap_or("")
}

#[derive(Debug, Clone, Copy)]
struct LastSig<'a> {
    kind: SyntaxKind,
    text: &'a str,
}

fn import_is_static(node: &SyntaxNode) -> bool {
    node.descendants_with_tokens()
        .filter_map(|e| e.into_token())
        .any(|tok| tok.kind() == SyntaxKind::StaticKw)
}

fn find_braces(body: &SyntaxNode) -> Option<(SyntaxToken, SyntaxToken)> {
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

fn has_blank_line_between_offsets(source: &str, start: u32, end: u32) -> bool {
    has_blank_line(source_slice_between_offsets(source, start, end))
}

fn has_whitespace_only_blank_line_between_offsets(source: &str, start: u32, end: u32) -> bool {
    let slice = source_slice_between_offsets(source, start, end);
    slice.trim().is_empty() && has_blank_line(slice)
}

fn source_slice_between_offsets(source: &str, start: u32, end: u32) -> &str {
    let len = source.len();
    let mut start = start as usize;
    let mut end = end as usize;
    start = start.min(len);
    end = end.min(len);
    if start > end {
        std::mem::swap(&mut start, &mut end);
    }
    source.get(start..end).unwrap_or("")
}

fn has_blank_line(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => i += 1,
            b'\r' => {
                i += 1;
                if i < bytes.len() && bytes[i] == b'\n' {
                    i += 1;
                }
            }
            _ => {
                i += 1;
                continue;
            }
        }

        let mut j = i;
        while j < bytes.len() && matches!(bytes[j], b' ' | b'\t') {
            j += 1;
        }
        if j < bytes.len() && matches!(bytes[j], b'\n' | b'\r') {
            return true;
        }
        i = j;
    }

    false
}

fn is_word_token(kind: SyntaxKind, text: &str) -> bool {
    // String templates lex their delimiters/text as dedicated tokens. These tokens should never
    // be treated as word-like for spacing heuristics: inserting spaces inside template payloads
    // can change semantics (e.g. `STR."Hello \{name}"`).
    if is_string_template_token(kind) {
        return false;
    }
    if matches!(
        kind,
        SyntaxKind::StringLiteral
            | SyntaxKind::CharLiteral
            | SyntaxKind::TextBlock
            | SyntaxKind::Number
            | SyntaxKind::IntLiteral
            | SyntaxKind::LongLiteral
            | SyntaxKind::FloatLiteral
            | SyntaxKind::DoubleLiteral
    ) {
        return true;
    }
    text.chars()
        .next()
        .is_some_and(|ch| ch.is_alphanumeric() || ch == '_' || ch == '$')
}

fn is_string_template_token(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::StringTemplateStart
            | SyntaxKind::StringTemplateText
            | SyntaxKind::StringTemplateExprStart
            | SyntaxKind::StringTemplateExprEnd
            | SyntaxKind::StringTemplateEnd
    )
}

fn is_control_keyword_kind(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::IfKw
            | SyntaxKind::ForKw
            | SyntaxKind::WhileKw
            | SyntaxKind::SwitchKw
            | SyntaxKind::CatchKw
            | SyntaxKind::SynchronizedKw
    )
}

fn needs_space_between(last: Option<&LastSig<'_>>, next_kind: SyntaxKind, next_text: &str) -> bool {
    let Some(last) = last else {
        return false;
    };

    // Do not apply generic whitespace heuristics within string template token streams: template
    // text segments can be arbitrary and may equal punctuation like `,` or `}`.
    if is_string_template_token(next_kind) || is_string_template_token(last.kind) {
        return false;
    }

    if matches!(
        next_kind,
        SyntaxKind::RParen
            | SyntaxKind::RBracket
            | SyntaxKind::RBrace
            | SyntaxKind::Semicolon
            | SyntaxKind::Comma
            | SyntaxKind::Dot
            | SyntaxKind::DoubleColon
    ) {
        return false;
    }
    if matches!(
        last.kind,
        SyntaxKind::LParen
            | SyntaxKind::LBracket
            | SyntaxKind::Dot
            | SyntaxKind::At
            | SyntaxKind::DoubleColon
    ) {
        return false;
    }
    if next_kind == SyntaxKind::At {
        // Do not insert whitespace between `<` and a type-argument annotation:
        // `List<@A T>` (not `List< @A T>`).
        if last.kind == SyntaxKind::Less {
            return false;
        }
        return true;
    }
    if is_control_keyword_kind(last.kind) && next_kind == SyntaxKind::LParen {
        return true;
    }
    if matches!(last.kind, SyntaxKind::Comma | SyntaxKind::Question) {
        return true;
    }

    if last.kind == SyntaxKind::RBracket && is_word_token(next_kind, next_text) {
        return true;
    }

    // Generic closes (and record header `)`) should be separated from following identifiers/keywords.
    if matches!(
        last.kind,
        SyntaxKind::Greater | SyntaxKind::RightShift | SyntaxKind::UnsignedRightShift
    ) && is_word_token(next_kind, next_text)
    {
        return true;
    }
    if matches!(last.kind, SyntaxKind::RParen | SyntaxKind::Ellipsis)
        && is_word_token(next_kind, next_text)
    {
        return true;
    }
    is_word_token(last.kind, last.text) && is_word_token(next_kind, next_text)
}

pub(crate) fn format_java_pretty(
    parse: &JavaParseResult,
    source: &str,
    config: &FormatConfig,
) -> String {
    // The doc-based pretty-printer is still experimental and intentionally
    // best-effort. On malformed input, prefer a no-op fallback rather than
    // risk dropping tokens/comments.
    //
    // Note: we use the parser's diagnostics here (rather than looking for
    // specific error tokens) so we preserve existing behavior from snapshot
    // tests like `pretty_formats_broken_syntax_without_panicking`.
    if !parse.errors.is_empty() {
        return source.to_string();
    }

    let newline = NewlineStyle::detect(source);
    let input_has_final_newline = ends_with_line_break(source);

    JavaPrettyFormatter::new(parse, source, config, newline).format(input_has_final_newline)
}

fn finalize_output(
    out: &mut String,
    config: &FormatConfig,
    input_has_final_newline: bool,
    newline: NewlineStyle,
) {
    let newline = newline.as_str();
    if config.trim_final_newlines == Some(true) {
        while matches!(out.as_bytes().last(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            out.pop();
        }
    }

    match config.insert_final_newline {
        Some(true) => {
            while matches!(out.as_bytes().last(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
                out.pop();
            }
            out.push_str(newline);
        }
        Some(false) => {
            while matches!(out.as_bytes().last(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
                out.pop();
            }
        }
        None => {
            if input_has_final_newline {
                // Trim trailing indentation/whitespace, but preserve any extra newlines already
                // present at EOF to keep legacy behavior stable.
                while matches!(out.as_bytes().last(), Some(b' ' | b'\t')) {
                    out.pop();
                }
                if !out.is_empty() && !out.ends_with(newline) {
                    if newline == "\r\n" && out.ends_with('\r') {
                        out.push('\n');
                    } else if out.ends_with('\n') && newline == "\r\n" {
                        out.pop();
                        out.push_str("\r\n");
                    } else {
                        out.push_str(newline);
                    }
                }
            } else {
                while matches!(out.as_bytes().last(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
                    out.pop();
                }
            }
        }
    }
}
