//! AST-aware Java formatting built on `nova_syntax::parse_java`.
//!
//! The formatter currently focuses on stable formatting for the compilation unit structure
//! (package / imports / top-level type declarations). It is intentionally best-effort and
//! must never panic on malformed input.

use nova_core::TextEdit;
use nova_syntax::{JavaParseResult, SyntaxKind, SyntaxToken};

use crate::{ends_with_line_break, minimal_text_edits, FormatConfig, NewlineStyle};

/// Format an entire Java source file using the rowan-based Java parser.
///
/// This is the preferred formatter entrypoint for full-document formatting. Range/on-type
/// formatting still uses the legacy token-only pipeline.
pub fn format_java_ast(parse: &JavaParseResult, source: &str, config: &FormatConfig) -> String {
    let newline = NewlineStyle::detect(source);
    let input_has_final_newline = ends_with_line_break(source);
    let mut out = format_compilation_unit(parse, source, config, newline);
    finalize_output(&mut out, config, input_has_final_newline, newline);

    // Ensure the formatter is idempotent on its own output, even when formatting
    // changes the parser's tokenization decisions on malformed inputs (e.g.
    // generic `>>` vs shift operators).
    //
    // Mirror the legacy token formatter's stabilization loop to guarantee that
    // the canonical full-document formatting pipeline reaches a fixed point.
    for _ in 0..8 {
        let reparsed = nova_syntax::parse_java(&out);
        let mut next = format_compilation_unit(&reparsed, &out, config, newline);
        finalize_output(&mut next, config, input_has_final_newline, newline);
        if next == out {
            break;
        }
        out = next;
    }

    out
}

/// Return minimal edits that transform `source` into its AST-formatted representation.
pub fn edits_for_formatting_ast(
    parse: &JavaParseResult,
    source: &str,
    config: &FormatConfig,
) -> Vec<TextEdit> {
    let formatted = format_java_ast(parse, source, config);
    minimal_text_edits(source, &formatted)
}

fn format_compilation_unit(
    parse: &JavaParseResult,
    source: &str,
    config: &FormatConfig,
    newline: NewlineStyle,
) -> String {
    let tokens: Vec<SyntaxToken> = parse
        .syntax()
        .descendants_with_tokens()
        .filter_map(|el| el.into_token())
        .filter(|tok| tok.kind() != SyntaxKind::Eof)
        .filter(|tok| !is_missing_token(tok.kind()))
        .collect();

    let mut out = String::new();
    let mut state = TokenFormatState::new(config, 0, newline);
    let mut sections = TopLevelSections::default();

    let mut idx = 0usize;
    while idx < tokens.len() {
        let token = &tokens[idx];
        let token_idx = idx;
        idx += 1;

        if token.kind() == SyntaxKind::Whitespace {
            // Discard existing whitespace, but preserve explicit blank lines so we don't
            // aggressively collapse user-separated top-level items.
            state.pending_blank_line |= count_line_breaks(token.text()) >= 2;
            continue;
        }

        if is_unterminated_lex_error(token) {
            // If the lexer encountered an unterminated string/comment/text block, formatting can
            // change the tokenization boundary on subsequent passes (e.g. by removing a newline
            // that terminated the recovery token). Stop formatting and preserve the remainder of
            // the file verbatim to keep the formatter deterministic on broken input.
            let start = u32::from(token.text_range().start()) as usize;
            if let Some(rest) = source.get(start..) {
                state.write_indent(&mut out);
                if needs_space_between(state.last_sig.as_ref(), token.kind(), token.text()) {
                    state.ensure_space(&mut out);
                }
                out.push_str(rest);
            } else {
                state.write_token(&mut out, &tokens, token_idx, token, None);
            }
            return out;
        }

        if token.kind() == SyntaxKind::StringTemplateStart {
            // String templates are preview syntax and can contain `{`/`}` characters that are not
            // Java block delimiters (notably the closing `}` of `\{...}` interpolations). The
            // token-walk formatter is intentionally conservative: treat templates as opaque and
            // preserve their full source text verbatim.
            let start_offset = u32::from(token.text_range().start()) as usize;
            let mut end_offset = u32::from(token.text_range().end()) as usize;

            let mut depth = 0u32;
            let mut scan = token_idx;
            while scan < tokens.len() {
                let tok = &tokens[scan];
                match tok.kind() {
                    SyntaxKind::StringTemplateStart => {
                        depth = depth.saturating_add(1);
                    }
                    SyntaxKind::StringTemplateEnd => {
                        depth = depth.saturating_sub(1);
                    }
                    _ => {}
                }
                end_offset = u32::from(tok.text_range().end()) as usize;
                scan += 1;
                if depth == 0 {
                    break;
                }
            }

            state.write_indent(&mut out);
            if needs_space_between(state.last_sig.as_ref(), token.kind(), token.text()) {
                state.ensure_space(&mut out);
            }

            if depth != 0 {
                // Unterminated template: preserve the remainder of the file verbatim for
                // determinism (mirrors `is_unterminated_lex_error` behavior for broken literals).
                let rest = source.get(start_offset..).unwrap_or("");
                out.push_str(rest);
                return out;
            }

            let slice = source.get(start_offset..end_offset).unwrap_or("");
            out.push_str(slice);

            let sig = SigToken::Token {
                kind: SyntaxKind::StringLiteral,
                text: slice.to_string(),
            };
            state.last_sig = Some(sig.clone());
            state.last_code_sig = Some(sig);
            state.pending_for = false;

            idx = scan;
            continue;
        }

        let at_top_level = state.indent_level == 0 && state.paren_depth == 0;

        if at_top_level && sections.pending_blank_after_imports {
            match token.kind() {
                SyntaxKind::ImportKw => {
                    // Another import continues the import section; keep comments (if any) between
                    // imports and avoid inserting a blank line until we reach the first non-import
                    // token.
                    sections.pending_blank_after_imports = false;
                }
                SyntaxKind::LineComment | SyntaxKind::BlockComment | SyntaxKind::DocComment => {
                    // Comments between imports belong to the import section; delay the blank line
                    // until we see the next non-import token.
                }
                _ => {
                    state.ensure_blank_line(&mut out);
                    sections.pending_blank_after_imports = false;
                }
            }
        }
        if at_top_level && sections.pending_blank_after_package {
            match token.kind() {
                SyntaxKind::LineComment | SyntaxKind::BlockComment | SyntaxKind::DocComment => {
                    // Treat comments following the package declaration as part of the package
                    // section and insert the separating blank line before the next declaration.
                }
                _ => {
                    state.ensure_blank_line(&mut out);
                    sections.pending_blank_after_package = false;
                }
            }
        }

        if state.pending_blank_line {
            state.ensure_blank_line(&mut out);
            state.pending_blank_line = false;
        }

        if at_top_level && token.kind() == SyntaxKind::ImportKw {
            let is_static = lookahead_import_is_static(&tokens, idx);
            if let Some(prev) = sections.last_import_static {
                if prev != is_static {
                    state.ensure_blank_line(&mut out);
                }
            }
            sections.in_import = true;
            sections.current_import_static = is_static;
            // Imports are still part of the import section; the blank-line-after-imports flag is
            // cleared above when we see the `import` keyword.
        } else if at_top_level && token.kind() == SyntaxKind::PackageKw {
            sections.in_package = true;
        }

        let next = next_significant(&tokens, idx);
        state.write_token(&mut out, &tokens, token_idx, token, next);

        if at_top_level && token.kind() == SyntaxKind::Semicolon {
            if sections.in_package {
                sections.in_package = false;
                sections.pending_blank_after_package = true;
            } else if sections.in_import {
                sections.in_import = false;
                sections.last_import_static = Some(sections.current_import_static);
                sections.pending_blank_after_imports = true;
            }
        }
    }

    // For node-level formatting we always terminate with a single newline; the compilation unit
    // formatter will adjust the final output based on the original file.
    state.ensure_newline(&mut out);
    out
}

#[derive(Debug, Default)]
struct TopLevelSections {
    in_package: bool,
    pending_blank_after_package: bool,
    in_import: bool,
    pending_blank_after_imports: bool,
    current_import_static: bool,
    last_import_static: Option<bool>,
}

fn is_missing_token(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::MissingSemicolon
            | SyntaxKind::MissingRParen
            | SyntaxKind::MissingRBrace
            | SyntaxKind::MissingRBracket
            | SyntaxKind::MissingGreater
    )
}

fn lookahead_import_is_static(tokens: &[SyntaxToken], mut idx: usize) -> bool {
    while idx < tokens.len() {
        match tokens[idx].kind() {
            SyntaxKind::Whitespace => idx += 1,
            SyntaxKind::StaticKw => return true,
            SyntaxKind::Semicolon => return false,
            _ => idx += 1,
        }
    }
    false
}

fn is_unterminated_lex_error(token: &SyntaxToken) -> bool {
    if token.kind() != SyntaxKind::Error {
        return false;
    }
    let text = token.text();
    text.starts_with("\"\"\"")
        || text.starts_with('"')
        || text.starts_with('\'')
        || text.starts_with("/*")
}

fn next_significant(tokens: &[SyntaxToken], mut idx: usize) -> Option<&SyntaxToken> {
    while idx < tokens.len() {
        if tokens[idx].kind() != SyntaxKind::Whitespace && !is_missing_token(tokens[idx].kind()) {
            return Some(&tokens[idx]);
        }
        idx += 1;
    }
    None
}

fn next_non_whitespace_with_breaks(
    tokens: &[SyntaxToken],
    mut idx: usize,
) -> (Option<&SyntaxToken>, bool) {
    let mut saw_line_break = false;
    idx = idx.saturating_add(1);
    while idx < tokens.len() {
        match tokens[idx].kind() {
            SyntaxKind::Whitespace => {
                saw_line_break |= count_line_breaks(tokens[idx].text()) > 0;
                idx += 1;
            }
            kind if is_missing_token(kind) => idx += 1,
            _ => return (Some(&tokens[idx]), saw_line_break),
        }
    }
    (None, saw_line_break)
}

fn ensure_newline(out: &mut String, newline: &'static str) {
    while matches!(out.as_bytes().last(), Some(b' ' | b'\t')) {
        out.pop();
    }

    if out.is_empty() {
        return;
    }

    if out.ends_with(newline) {
        return;
    }

    // Avoid producing `\r\r\n` if the output already ends with a lone `\r`.
    if newline == "\r\n" {
        if out.ends_with('\r') {
            out.push('\n');
        } else if out.ends_with('\n') {
            out.pop();
            out.push_str("\r\n");
        } else {
            out.push_str("\r\n");
        }
    } else if newline == "\n" {
        if out.ends_with("\r\n") {
            out.pop(); // '\n'
            out.pop(); // '\r'
            out.push('\n');
        } else if out.ends_with('\r') {
            out.pop();
            out.push('\n');
        } else if !out.ends_with('\n') {
            out.push('\n');
        }
    } else if newline == "\r" {
        if out.ends_with("\r\n") {
            out.pop(); // '\n'
        } else if out.ends_with('\n') {
            out.pop();
        }
        if !out.ends_with('\r') {
            out.push('\r');
        }
    }
}

fn ensure_blank_line(out: &mut String, newline: &'static str) {
    if out.is_empty() {
        return;
    }
    ensure_newline(out, newline);
    let nl_len = newline.len();
    let has_blank_line = out.len() >= nl_len * 2
        && out.ends_with(newline)
        && out[..out.len() - nl_len].ends_with(newline);
    if !has_blank_line {
        out.push_str(newline);
    }
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

fn count_line_breaks(text: &str) -> u32 {
    let bytes = text.as_bytes();
    let mut i = 0usize;
    let mut count = 0u32;

    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                count += 1;
                i += 1;
            }
            b'\r' => {
                count += 1;
                i += 1;
                if i < bytes.len() && bytes[i] == b'\n' {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }

    count
}

#[derive(Debug, Clone)]
struct TokenFormatState<'a> {
    config: &'a FormatConfig,
    indent_level: usize,
    at_line_start: bool,
    pending_blank_line: bool,
    paren_depth: usize,
    for_paren_depth: Option<usize>,
    pending_for: bool,
    last_sig: Option<SigToken>,
    /// The most recent non-comment significant token, used for generic disambiguation.
    last_code_sig: Option<SigToken>,
    generic_stack: Vec<GenericContext>,
    newline: &'static str,
}

#[derive(Debug, Clone)]
enum SigToken {
    Token { kind: SyntaxKind, text: String },
    GenericClose { kind: SyntaxKind, after_dot: bool },
}

impl SigToken {
    fn kind(&self) -> Option<SyntaxKind> {
        match self {
            SigToken::Token { kind, .. } => Some(*kind),
            SigToken::GenericClose { kind, .. } => Some(*kind),
        }
    }

    fn text(&self) -> &str {
        match self {
            SigToken::Token { text, .. } => text,
            SigToken::GenericClose { kind, .. } => match kind {
                SyntaxKind::RightShift => ">>",
                SyntaxKind::UnsignedRightShift => ">>>",
                _ => ">",
            },
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct GenericContext {
    after_dot: bool,
}

impl<'a> TokenFormatState<'a> {
    fn new(config: &'a FormatConfig, initial_indent: usize, newline: NewlineStyle) -> Self {
        Self {
            config,
            indent_level: initial_indent,
            at_line_start: true,
            pending_blank_line: false,
            paren_depth: 0,
            for_paren_depth: None,
            pending_for: false,
            last_sig: None,
            last_code_sig: None,
            generic_stack: Vec::new(),
            newline: newline.as_str(),
        }
    }

    fn ensure_newline(&mut self, out: &mut String) {
        ensure_newline(out, self.newline);
        self.at_line_start = true;
        self.last_sig = None;
        self.last_code_sig = None;
    }

    fn ensure_blank_line(&mut self, out: &mut String) {
        ensure_blank_line(out, self.newline);
        self.at_line_start = true;
        self.last_sig = None;
        self.last_code_sig = None;
    }

    fn write_indent(&mut self, out: &mut String) {
        if !self.at_line_start {
            return;
        }
        out.push_str(&crate::indentation_for(self.config, self.indent_level));
        self.at_line_start = false;
    }

    fn ensure_space(&mut self, out: &mut String) {
        if self.at_line_start {
            return;
        }
        if matches!(
            out.as_bytes().last(),
            None | Some(b' ' | b'\n' | b'\r' | b'\t')
        ) {
            return;
        }
        out.push(' ');
    }

    fn push_hardline(&mut self, out: &mut String) {
        while matches!(out.as_bytes().last(), Some(b' ' | b'\t')) {
            out.pop();
        }
        out.push_str(self.newline);
        self.at_line_start = true;
        self.last_sig = None;
        self.last_code_sig = None;
    }

    fn generic_depth(&self) -> usize {
        self.generic_stack.len()
    }

    fn pop_generic(&mut self, count: usize) -> bool {
        let mut after_dot = false;
        for _ in 0..count {
            if let Some(ctx) = self.generic_stack.pop() {
                after_dot = ctx.after_dot;
            } else {
                break;
            }
        }
        after_dot
    }

    fn write_token(
        &mut self,
        out: &mut String,
        tokens: &[SyntaxToken],
        idx: usize,
        token: &SyntaxToken,
        next: Option<&SyntaxToken>,
    ) {
        let kind = token.kind();
        let text = token.text();
        let next_kind = next.map(|t| t.kind());

        match kind {
            SyntaxKind::LineComment => {
                self.write_indent(out);
                if self.last_sig.is_some() {
                    self.ensure_space(out);
                }
                out.push_str(text.trim_end_matches(['\r', '\n']));
                self.ensure_newline(out);
                self.last_sig = None;
                self.last_code_sig = None;
                self.pending_for = false;
            }
            SyntaxKind::DocComment => {
                self.write_indent(out);
                if self.last_sig.is_some() {
                    self.ensure_space(out);
                }
                self.write_block_comment(out, text);
                self.ensure_newline(out);
                self.last_sig = None;
                self.last_code_sig = None;
                self.pending_for = false;
            }
            SyntaxKind::BlockComment => {
                self.write_indent(out);
                if self.last_sig.is_some() {
                    self.ensure_space(out);
                }
                self.write_block_comment(out, text);
                self.last_sig = Some(SigToken::Token {
                    kind,
                    text: text.to_string(),
                });
                self.pending_for = false;
            }
            SyntaxKind::Less => {
                let prev_sig = if self.at_line_start {
                    None
                } else {
                    self.last_code_sig.clone()
                };
                let starts_generic = should_start_generic(tokens, idx, prev_sig.as_ref());
                self.write_indent(out);
                if starts_generic {
                    if prev_sig
                        .as_ref()
                        .is_some_and(|sig| sig.kind().is_some_and(|k| k.is_modifier_keyword()))
                    {
                        self.ensure_space(out);
                    }
                    out.push_str(text);
                    self.generic_stack.push(GenericContext {
                        after_dot: prev_sig.as_ref().is_some_and(|sig| {
                            sig.kind().is_some_and(|k| {
                                matches!(k, SyntaxKind::Dot | SyntaxKind::DoubleColon)
                            })
                        }),
                    });
                } else {
                    if needs_space_between(self.last_sig.as_ref(), kind, text) {
                        self.ensure_space(out);
                    }
                    out.push_str(text);
                }
                let sig = SigToken::Token {
                    kind,
                    text: text.to_string(),
                };
                self.last_sig = Some(sig.clone());
                self.last_code_sig = Some(sig);
                self.pending_for = false;
            }
            SyntaxKind::Greater | SyntaxKind::RightShift | SyntaxKind::UnsignedRightShift
                if self.generic_depth() > 0 =>
            {
                self.write_indent(out);
                if let Some(last) = self.last_sig.as_ref() {
                    // Some `>`-family tokens are split/retokenized by the rowan Java parser in
                    // generic contexts (e.g. a `>>` close may surface as two `>` tokens). Only
                    // insert a separator when the *source* contained trivia between the tokens;
                    // otherwise we'd turn an existing `>>` into `> >` and break idempotence.
                    if needs_space_to_avoid_token_merge(last, kind)
                        && idx > 0
                        && matches!(
                            tokens.get(idx - 1).map(|t| t.kind()),
                            Some(SyntaxKind::Whitespace)
                        )
                    {
                        self.ensure_space(out);
                    }
                }
                out.push_str(text);
                let after_dot = match kind {
                    SyntaxKind::Greater => self.pop_generic(1),
                    SyntaxKind::RightShift => self.pop_generic(2),
                    SyntaxKind::UnsignedRightShift => self.pop_generic(3),
                    _ => false,
                };
                let sig = SigToken::GenericClose { kind, after_dot };
                self.last_sig = Some(sig.clone());
                self.last_code_sig = Some(sig);
                self.pending_for = false;
            }
            SyntaxKind::Question if self.generic_depth() > 0 => {
                self.write_indent(out);
                if needs_space_between(self.last_sig.as_ref(), kind, text) {
                    self.ensure_space(out);
                }
                out.push_str(text);
                if let Some(next) = next {
                    if next.kind() == SyntaxKind::At || is_word_token(next.kind(), next.text()) {
                        self.ensure_space(out);
                    }
                }
                let sig = SigToken::Token {
                    kind,
                    text: text.to_string(),
                };
                self.last_sig = Some(sig.clone());
                self.last_code_sig = Some(sig);
                self.pending_for = false;
            }
            SyntaxKind::Ellipsis => {
                self.write_indent(out);
                if needs_space_between(self.last_sig.as_ref(), kind, text) {
                    self.ensure_space(out);
                }
                out.push_str(text);
                if let Some(next) = next {
                    if next.kind() == SyntaxKind::At || is_word_token(next.kind(), next.text()) {
                        self.ensure_space(out);
                    }
                }
                let sig = SigToken::Token {
                    kind,
                    text: text.to_string(),
                };
                self.last_sig = Some(sig.clone());
                self.last_code_sig = Some(sig);
                self.pending_for = false;
            }
            SyntaxKind::LBrace => {
                self.write_indent(out);
                if needs_space_before(self.last_sig.as_ref(), kind) {
                    self.ensure_space(out);
                }
                out.push('{');
                self.ensure_newline(out);
                self.indent_level = self.indent_level.saturating_add(1);
                self.last_sig = None;
                self.last_code_sig = None;
                self.pending_for = false;
            }
            SyntaxKind::RBrace => {
                self.indent_level = self.indent_level.saturating_sub(1);
                self.ensure_newline(out);
                self.write_indent(out);
                out.push('}');

                let (next_non_ws, saw_line_break) = next_non_whitespace_with_breaks(tokens, idx);
                let next_kind = next_non_ws.map(|t| t.kind());

                let join_next = matches!(
                    next_kind,
                    Some(
                        SyntaxKind::ElseKw
                            | SyntaxKind::CatchKw
                            | SyntaxKind::FinallyKw
                            | SyntaxKind::WhileKw
                    )
                ) || matches!(
                    next_kind,
                    Some(
                        SyntaxKind::Semicolon
                            | SyntaxKind::Comma
                            | SyntaxKind::RParen
                            | SyntaxKind::RBracket
                    )
                ) || matches!(next_kind, Some(SyntaxKind::LineComment))
                    && !saw_line_break;
                if matches!(
                    next_kind,
                    Some(
                        SyntaxKind::ElseKw
                            | SyntaxKind::CatchKw
                            | SyntaxKind::FinallyKw
                            | SyntaxKind::WhileKw
                    )
                ) {
                    self.ensure_space(out);
                } else if !join_next {
                    self.ensure_newline(out);
                }

                let sig = SigToken::Token {
                    kind,
                    text: "}".to_string(),
                };
                self.last_sig = Some(sig.clone());
                self.last_code_sig = Some(sig);
                self.pending_for = false;
            }
            SyntaxKind::Semicolon => {
                self.write_indent(out);
                out.push(';');

                let in_for_header = self
                    .for_paren_depth
                    .is_some_and(|depth| self.paren_depth >= depth);
                let (next_non_ws, saw_line_break) = next_non_whitespace_with_breaks(tokens, idx);
                let next_kind = next_non_ws.map(|t| t.kind());
                let next_is_comment = matches!(
                    next_kind,
                    Some(
                        SyntaxKind::LineComment | SyntaxKind::BlockComment | SyntaxKind::DocComment
                    )
                );

                if next_is_comment && !saw_line_break {
                    // Keep trailing comments on the same line.
                } else if in_for_header && !next_is_comment {
                    if next_non_ws.is_some()
                        && !matches!(next_kind, Some(SyntaxKind::RParen | SyntaxKind::Semicolon))
                    {
                        self.ensure_space(out);
                    }
                } else {
                    self.ensure_newline(out);
                }

                let sig = SigToken::Token {
                    kind,
                    text: ";".to_string(),
                };
                self.last_sig = Some(sig.clone());
                self.last_code_sig = Some(sig);
                self.pending_for = false;
            }
            SyntaxKind::Comma => {
                self.write_indent(out);
                out.push(',');
                if next.is_some()
                    && !matches!(next_kind, Some(SyntaxKind::RParen | SyntaxKind::RBracket))
                {
                    self.ensure_space(out);
                }
                let sig = SigToken::Token {
                    kind,
                    text: ",".to_string(),
                };
                self.last_sig = Some(sig.clone());
                self.last_code_sig = Some(sig);
                self.pending_for = false;
            }
            SyntaxKind::LParen => {
                self.write_indent(out);
                if needs_space_before(self.last_sig.as_ref(), kind) {
                    self.ensure_space(out);
                }
                out.push('(');
                self.paren_depth = self.paren_depth.saturating_add(1);
                if self.pending_for {
                    self.for_paren_depth = Some(self.paren_depth);
                    self.pending_for = false;
                }
                let sig = SigToken::Token {
                    kind,
                    text: "(".to_string(),
                };
                self.last_sig = Some(sig.clone());
                self.last_code_sig = Some(sig);
            }
            SyntaxKind::RParen => {
                self.write_indent(out);
                out.push(')');
                if let Some(depth) = self.for_paren_depth {
                    if depth == self.paren_depth {
                        self.for_paren_depth = None;
                    }
                }
                self.paren_depth = self.paren_depth.saturating_sub(1);
                let sig = SigToken::Token {
                    kind,
                    text: ")".to_string(),
                };
                self.last_sig = Some(sig.clone());
                self.last_code_sig = Some(sig);
                self.pending_for = false;
            }
            SyntaxKind::ForKw => {
                self.write_indent(out);
                if needs_space_between(self.last_sig.as_ref(), kind, text) {
                    self.ensure_space(out);
                }
                out.push_str(text);
                let sig = SigToken::Token {
                    kind,
                    text: text.to_string(),
                };
                self.last_sig = Some(sig.clone());
                self.last_code_sig = Some(sig);
                self.pending_for = true;
            }
            _ => {
                self.write_indent(out);
                if needs_space_between(self.last_sig.as_ref(), kind, text) {
                    self.ensure_space(out);
                }

                out.push_str(text);
                let sig = SigToken::Token {
                    kind,
                    text: text.to_string(),
                };
                self.last_sig = Some(sig.clone());
                self.last_code_sig = Some(sig);
                self.pending_for = false;
            }
        }
    }

    fn write_block_comment(&mut self, out: &mut String, text: &str) {
        if !crate::comment_printer::comment_contains_line_break(text) {
            out.push_str(text);
            return;
        }

        let lines = crate::comment_printer::split_lines(text);
        if lines.is_empty() {
            return;
        }

        let common = crate::comment_printer::common_indent(lines.iter().skip(1).map(|l| l.text));

        for (idx, line) in lines.iter().enumerate() {
            if idx == 0 {
                out.push_str(line.text);
            } else {
                self.write_indent(out);
                let trimmed = crate::comment_printer::trim_indent(line.text, common);
                out.push_str(trimmed);
            }

            if line.has_line_break {
                self.push_hardline(out);
            }
        }
    }
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

fn needs_space_before(last: Option<&SigToken>, next_kind: SyntaxKind) -> bool {
    let Some(last) = last else {
        return false;
    };

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
        last.kind(),
        Some(
            SyntaxKind::LParen
                | SyntaxKind::LBracket
                | SyntaxKind::Less
                | SyntaxKind::Dot
                | SyntaxKind::At
                | SyntaxKind::DoubleColon
        )
    ) {
        return false;
    }

    if last.kind().is_some_and(is_control_keyword_kind) && next_kind == SyntaxKind::LParen {
        return true;
    }

    if next_kind == SyntaxKind::LBrace {
        return !matches!(
            last.kind(),
            Some(
                SyntaxKind::LParen
                    | SyntaxKind::LBracket
                    | SyntaxKind::Dot
                    | SyntaxKind::At
                    | SyntaxKind::DoubleColon
            )
        );
    }

    match last {
        SigToken::Token { kind, text } => {
            is_word_token(*kind, text) && next_kind != SyntaxKind::LParen
        }
        SigToken::GenericClose { .. } => false,
    }
}

fn needs_space_between(last: Option<&SigToken>, next_kind: SyntaxKind, next_text: &str) -> bool {
    let Some(last) = last else {
        return false;
    };

    if is_string_template_token(next_kind) || last.kind().is_some_and(is_string_template_token) {
        return false;
    }

    if needs_space_to_avoid_token_merge(last, next_kind) {
        return true;
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
        last.kind(),
        Some(
            SyntaxKind::LParen
                | SyntaxKind::LBracket
                | SyntaxKind::Less
                | SyntaxKind::Dot
                | SyntaxKind::At
                | SyntaxKind::DoubleColon
        )
    ) {
        return false;
    }

    let last_kind = last.kind().unwrap_or(SyntaxKind::Error);
    if is_assignment_operator_kind(next_kind) || is_assignment_operator_kind(last_kind) {
        return true;
    }
    if next_kind == SyntaxKind::At {
        return true;
    }
    if is_control_keyword_kind(last_kind) && next_kind == SyntaxKind::LParen {
        return true;
    }
    if last_kind == SyntaxKind::Comma {
        return true;
    }

    if last_kind == SyntaxKind::RBracket && is_word_token(next_kind, next_text) {
        return true;
    }
    match last {
        SigToken::GenericClose { after_dot, .. } => {
            if *after_dot {
                return false;
            }
            // `List<String> foo` but not `List<String>()` / `List<String>[]` / `foo.<T>bar`.
            if matches!(next_kind, SyntaxKind::LParen | SyntaxKind::LBracket) {
                return false;
            }
            is_word_token(next_kind, next_text)
        }
        SigToken::Token { kind, text } => {
            is_word_token(*kind, text) && is_word_token(next_kind, next_text)
        }
    }
}

fn is_assignment_operator_kind(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::Eq
            | SyntaxKind::PlusEq
            | SyntaxKind::MinusEq
            | SyntaxKind::StarEq
            | SyntaxKind::SlashEq
            | SyntaxKind::PercentEq
            | SyntaxKind::AmpEq
            | SyntaxKind::PipeEq
            | SyntaxKind::CaretEq
            | SyntaxKind::LeftShiftEq
            | SyntaxKind::RightShiftEq
            | SyntaxKind::UnsignedRightShiftEq
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

fn needs_space_to_avoid_token_merge(last: &SigToken, next_kind: SyntaxKind) -> bool {
    let (last_kind, last_text) = match last {
        SigToken::Token { kind, text } => (*kind, text.as_str()),
        SigToken::GenericClose { kind, .. } => (*kind, last.text()),
    };

    if is_numeric_literal_kind(last_kind) && next_kind == SyntaxKind::Dot {
        return true;
    }
    if last_kind == SyntaxKind::Dot && is_numeric_literal_kind(next_kind) {
        return true;
    }

    // Avoid synthesizing the `non-sealed` restricted keyword from spaced `non - sealed` tokens.
    if last_kind == SyntaxKind::Identifier && last_text == "non" && next_kind == SyntaxKind::Minus {
        return true;
    }

    match (last_kind, next_kind) {
        // Keep `:` tokens separated so we don't accidentally create a `::` method reference token
        // when the input contains `: :`.
        (SyntaxKind::Colon, SyntaxKind::Colon) => true,

        // Prevent sequences of dot tokens (e.g. `. . .`) from collapsing into the `...` ellipsis
        // token on the next parse.
        (SyntaxKind::Dot, SyntaxKind::Dot) => true,

        // Avoid producing comment tokens like `//` or `/*` from separate `/` + `/` / `*` tokens.
        (SyntaxKind::Slash, SyntaxKind::Slash | SyntaxKind::Star | SyntaxKind::SlashEq) => true,

        // Avoid turning `- >` into the `->` arrow token.
        (SyntaxKind::Minus, SyntaxKind::Greater) => true,
        // Avoid turning `- ->` into `-->` (which would tokenize as `--` + `>`).
        (SyntaxKind::Minus, SyntaxKind::Arrow) => true,

        // Avoid merging standalone operators into their combined forms when the input separated
        // them with whitespace (e.g. `+ +` -> `++`).
        (SyntaxKind::Plus, SyntaxKind::Plus) => true,
        // `+ ++` -> `+++` (tokenizes as `++` + `+`).
        (SyntaxKind::Plus, SyntaxKind::PlusPlus) => true,
        // `+ +=` -> `++=` (tokenizes as `++` + `=`).
        (SyntaxKind::Plus, SyntaxKind::PlusEq) => true,
        (SyntaxKind::Minus, SyntaxKind::Minus) => true,
        // `- --` -> `---` (tokenizes as `--` + `-`).
        (SyntaxKind::Minus, SyntaxKind::MinusMinus) => true,
        // `- -=` -> `--=` (tokenizes as `--` + `=`).
        (SyntaxKind::Minus, SyntaxKind::MinusEq) => true,
        (SyntaxKind::Amp, SyntaxKind::Amp) => true,
        // `& &&` -> `&&&` (tokenizes as `&&` + `&`).
        (SyntaxKind::Amp, SyntaxKind::AmpAmp) => true,
        // `& &=` -> `&&=` (tokenizes as `&&` + `=`).
        (SyntaxKind::Amp, SyntaxKind::AmpEq) => true,
        (SyntaxKind::Pipe, SyntaxKind::Pipe) => true,
        // `| ||` -> `|||` (tokenizes as `||` + `|`).
        (SyntaxKind::Pipe, SyntaxKind::PipePipe) => true,
        // `| |=` -> `||=` (tokenizes as `||` + `=`).
        (SyntaxKind::Pipe, SyntaxKind::PipeEq) => true,
        (SyntaxKind::Eq, SyntaxKind::Eq) => true,
        (SyntaxKind::Bang, SyntaxKind::Eq) => true,
        (SyntaxKind::Less, SyntaxKind::Less) => true,
        (SyntaxKind::Greater, SyntaxKind::Greater) => true,
        // `< <<` / `< <<=` -> `<<<` / `<<<=` (tokenizes as `<<` + `<` / `<<` + `<=`).
        (SyntaxKind::Less, SyntaxKind::LeftShift | SyntaxKind::LeftShiftEq) => true,
        // Avoid collapsing separated shift tokens like `> >>` or `>> >` into the unsigned-shift
        // operator `>>>` (or similarly `> >=` -> `>>=`).
        (SyntaxKind::Greater, SyntaxKind::RightShift)
        | (SyntaxKind::RightShift, SyntaxKind::Greater)
        // `> >>>` / `> >>>=` -> `>>>>` / `>>>>=` (tokenizes as `>>>` + `>` / `>>>` + `>=`).
        | (SyntaxKind::Greater, SyntaxKind::UnsignedRightShift | SyntaxKind::UnsignedRightShiftEq)
        | (SyntaxKind::Greater, SyntaxKind::GreaterEq)
        | (SyntaxKind::RightShift, SyntaxKind::GreaterEq)
        | (SyntaxKind::Greater, SyntaxKind::RightShiftEq) => true,
        // `>> >>` / `>> >>>` / `>> >>>=` would change tokenization (`>>>>` -> `>>>` + `>`).
        (SyntaxKind::RightShift, SyntaxKind::RightShift)
        | (SyntaxKind::RightShift, SyntaxKind::UnsignedRightShift | SyntaxKind::UnsignedRightShiftEq) => true,
        // Similarly for left-shift assignment: `< <=` would become `<<=`.
        (SyntaxKind::Less, SyntaxKind::LessEq) => true,

        // Avoid forming assignment operators like `+=` / `>>=` from separated tokens.
        (
            SyntaxKind::Plus
            | SyntaxKind::Minus
            | SyntaxKind::Star
            | SyntaxKind::Slash
            | SyntaxKind::Percent
            | SyntaxKind::Amp
            | SyntaxKind::Pipe
            | SyntaxKind::Caret
            | SyntaxKind::Less
            | SyntaxKind::Greater
            | SyntaxKind::LeftShift
            | SyntaxKind::RightShift
            | SyntaxKind::UnsignedRightShift,
            SyntaxKind::Eq,
        ) => true,

        _ => false,
    }
}

fn is_numeric_literal_kind(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::Number
            | SyntaxKind::IntLiteral
            | SyntaxKind::LongLiteral
            | SyntaxKind::FloatLiteral
            | SyntaxKind::DoubleLiteral
    )
}

fn should_start_generic(tokens: &[SyntaxToken], idx: usize, prev: Option<&SigToken>) -> bool {
    let next = next_non_trivia(tokens, idx + 1);
    let next2 = next_non_trivia(tokens, idx + 2);
    let Some(next) = next else {
        return false;
    };

    let prev_allows = match prev {
        None => true,
        Some(SigToken::GenericClose { .. }) => true,
        Some(SigToken::Token { kind, text }) => match kind {
            SyntaxKind::Dot | SyntaxKind::DoubleColon => true,
            _ if kind.is_modifier_keyword() || *kind == SyntaxKind::NewKw => true,
            _ if kind.is_identifier_like() => looks_like_type_name(text),
            SyntaxKind::RBracket
            | SyntaxKind::RParen
            | SyntaxKind::Greater
            | SyntaxKind::RightShift
            | SyntaxKind::UnsignedRightShift => true,
            _ => false,
        },
    };

    if !prev_allows {
        return false;
    }

    // Diamond operator `<>` in `new Foo<>()`.
    if next.kind() == SyntaxKind::Greater {
        return true;
    }

    let next_is_typeish = match next.kind() {
        SyntaxKind::Question | SyntaxKind::At => true,
        kind if kind.is_identifier_like() => {
            looks_like_type_name(next.text())
                || matches!(next2.map(|t| t.kind()), Some(SyntaxKind::Dot))
        }
        _ => false,
    };

    if !next_is_typeish {
        return false;
    }

    // If the type argument begins with an identifier, require that we see a matching `>` before we
    // hit disqualifying expression punctuation. This prevents treating comparisons like
    // `MAX < MIN` as generics when there is no closing `>`.
    if next.kind().is_identifier_like() && !has_generic_close_ahead(tokens, idx) {
        return false;
    }

    true
}

fn next_non_trivia(tokens: &[SyntaxToken], mut idx: usize) -> Option<&SyntaxToken> {
    while idx < tokens.len() {
        match tokens[idx].kind() {
            SyntaxKind::Whitespace
            | SyntaxKind::LineComment
            | SyntaxKind::BlockComment
            | SyntaxKind::DocComment => {
                idx += 1;
            }
            _ => return Some(&tokens[idx]),
        }
    }
    None
}

fn has_generic_close_ahead(tokens: &[SyntaxToken], l_angle_idx: usize) -> bool {
    let mut generic_depth: usize = 1;
    let mut paren_depth: usize = 0;
    let mut bracket_depth: usize = 0;
    let mut brace_depth: usize = 0;

    // Limit lookahead to keep the formatter linear-ish even on pathological input.
    let limit = 256usize;
    for (steps, tok) in tokens.iter().skip(l_angle_idx + 1).enumerate() {
        if steps >= limit {
            break;
        }

        match tok.kind() {
            SyntaxKind::Whitespace
            | SyntaxKind::LineComment
            | SyntaxKind::BlockComment
            | SyntaxKind::DocComment => continue,
            _ => {}
        }

        let is_top_level = paren_depth == 0 && bracket_depth == 0 && brace_depth == 0;

        match tok.kind() {
            SyntaxKind::LParen => paren_depth += 1,
            SyntaxKind::RParen => {
                if paren_depth == 0 {
                    return false;
                }
                paren_depth -= 1;
            }
            SyntaxKind::LBracket => bracket_depth += 1,
            SyntaxKind::RBracket => {
                if bracket_depth == 0 {
                    return false;
                }
                bracket_depth -= 1;
            }
            SyntaxKind::LBrace => brace_depth += 1,
            SyntaxKind::RBrace => {
                if brace_depth == 0 {
                    return false;
                }
                brace_depth -= 1;
            }
            SyntaxKind::Less if is_top_level => {
                generic_depth = generic_depth.saturating_add(1);
            }
            SyntaxKind::Greater if is_top_level => {
                generic_depth = generic_depth.saturating_sub(1);
                if generic_depth == 0 {
                    return true;
                }
            }
            SyntaxKind::RightShift if is_top_level => {
                // A `>>` token can only act as a generic close when we are at depth >= 2 (nested
                // type arguments). Treating `>>` as a close at depth 1 would misclassify common
                // shift expressions like `MAX < MIN >> 1` as generics.
                if generic_depth < 2 {
                    return false;
                }
                generic_depth = generic_depth.saturating_sub(2);
                if generic_depth == 0 {
                    return true;
                }
            }
            SyntaxKind::UnsignedRightShift if is_top_level => {
                // Same reasoning as `>>`: `>>>` only closes generics when we are nested to at
                // least depth 3.
                if generic_depth < 3 {
                    return false;
                }
                generic_depth = generic_depth.saturating_sub(3);
                if generic_depth == 0 {
                    return true;
                }
            }
            kind if is_top_level && is_disqualifying_generic_punct(kind) => return false,
            _ => {}
        }
    }

    false
}

fn is_disqualifying_generic_punct(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::Semicolon
            | SyntaxKind::Colon
            | SyntaxKind::Eq
            | SyntaxKind::EqEq
            | SyntaxKind::BangEq
            | SyntaxKind::LessEq
            | SyntaxKind::GreaterEq
            | SyntaxKind::AmpAmp
            | SyntaxKind::PipePipe
            | SyntaxKind::Plus
            | SyntaxKind::Minus
            | SyntaxKind::Star
            | SyntaxKind::Slash
            | SyntaxKind::Percent
            | SyntaxKind::Caret
            | SyntaxKind::Pipe
            | SyntaxKind::Bang
            | SyntaxKind::Tilde
            | SyntaxKind::PlusPlus
            | SyntaxKind::MinusMinus
            | SyntaxKind::LeftShift
            | SyntaxKind::RightShiftEq
            | SyntaxKind::UnsignedRightShiftEq
            | SyntaxKind::LeftShiftEq
            | SyntaxKind::PlusEq
            | SyntaxKind::MinusEq
            | SyntaxKind::StarEq
            | SyntaxKind::SlashEq
            | SyntaxKind::PercentEq
            | SyntaxKind::AmpEq
            | SyntaxKind::PipeEq
            | SyntaxKind::CaretEq
            | SyntaxKind::Arrow
    )
}

fn looks_like_type_name(text: &str) -> bool {
    text.chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_uppercase())
}
