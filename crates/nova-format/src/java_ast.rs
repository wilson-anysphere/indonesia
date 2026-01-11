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
    let mut out = format_compilation_unit(parse, config, newline);

    finalize_output(&mut out, config, input_has_final_newline, newline);

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
        idx += 1;

        if token.kind() == SyntaxKind::Whitespace {
            // Discard existing whitespace, but preserve explicit blank lines so we don't
            // aggressively collapse user-separated top-level items.
            state.pending_blank_line |= count_line_breaks(token.text()) >= 2;
            continue;
        }

        let at_top_level = state.indent_level == 0 && state.paren_depth == 0;

        if at_top_level && sections.pending_blank_after_imports {
            if token.kind() != SyntaxKind::ImportKw {
                state.ensure_blank_line(&mut out);
            }
            sections.pending_blank_after_imports = false;
        }
        if at_top_level && sections.pending_blank_after_package {
            state.ensure_blank_line(&mut out);
            sections.pending_blank_after_package = false;
        }

        if state.pending_blank_line {
            state.ensure_blank_line(&mut out);
            state.pending_blank_line = false;
        }

        let text = token.text();

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

        let next = next_significant(&tokens, idx).map(|t| t.text());
        state.write_token(&mut out, token.kind(), text, next);

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

fn next_significant(tokens: &[SyntaxToken], mut idx: usize) -> Option<&SyntaxToken> {
    while idx < tokens.len() {
        if tokens[idx].kind() != SyntaxKind::Whitespace && !is_missing_token(tokens[idx].kind()) {
            return Some(&tokens[idx]);
        }
        idx += 1;
    }
    None
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
    last_sig: Option<LastToken>,
    newline: &'static str,
}

#[derive(Debug, Clone)]
struct LastToken {
    kind: SyntaxKind,
    text: String,
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
            newline: newline.as_str(),
        }
    }

    fn ensure_newline(&mut self, out: &mut String) {
        ensure_newline(out, self.newline);
        self.at_line_start = true;
    }

    fn ensure_blank_line(&mut self, out: &mut String) {
        ensure_blank_line(out, self.newline);
        self.at_line_start = true;
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

    fn write_token(&mut self, out: &mut String, kind: SyntaxKind, text: &str, next: Option<&str>) {
        match kind {
            SyntaxKind::LineComment => {
                self.write_indent(out);
                if self.last_sig.is_some() {
                    self.ensure_space(out);
                }
                out.push_str(text.trim_end_matches(['\r', '\n']));
                self.ensure_newline(out);
                self.last_sig = None;
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
                self.pending_for = false;
            }
            SyntaxKind::BlockComment => {
                self.write_indent(out);
                if self.last_sig.is_some() {
                    self.ensure_space(out);
                }
                self.write_block_comment(out, text);
                self.last_sig = Some(LastToken {
                    kind,
                    text: text.to_string(),
                });
                self.pending_for = false;
            }
            _ if text == "{" => {
                self.write_indent(out);
                if needs_space_before(self.last_sig.as_ref(), text) {
                    self.ensure_space(out);
                }
                out.push('{');
                self.ensure_newline(out);
                self.indent_level = self.indent_level.saturating_add(1);
                self.last_sig = None;
                self.pending_for = false;
            }
            _ if text == "}" => {
                self.indent_level = self.indent_level.saturating_sub(1);
                self.ensure_newline(out);
                self.write_indent(out);
                out.push('}');

                let join_next = matches!(next, Some("else" | "catch" | "finally" | "while"))
                    || matches!(next, Some(";") | Some(",") | Some(")") | Some("]"));
                if matches!(next, Some("else" | "catch" | "finally" | "while")) {
                    self.ensure_space(out);
                } else if !join_next {
                    self.ensure_newline(out);
                }

                self.last_sig = Some(LastToken {
                    kind,
                    text: "}".to_string(),
                });
                self.pending_for = false;
            }
            _ if text == ";" => {
                self.write_indent(out);
                out.push(';');

                let in_for_header = self
                    .for_paren_depth
                    .is_some_and(|depth| self.paren_depth >= depth);
                let next_is_comment =
                    matches!(next, Some(s) if s.starts_with("//") || s.starts_with("/*"));
                if next_is_comment {
                    // Keep trailing comments on the same line.
                } else if in_for_header {
                    if next.is_some() && !matches!(next, Some(")") | Some(";")) {
                        self.ensure_space(out);
                    }
                } else {
                    self.ensure_newline(out);
                }

                self.last_sig = Some(LastToken {
                    kind,
                    text: ";".to_string(),
                });
                self.pending_for = false;
            }
            _ if text == "," => {
                self.write_indent(out);
                out.push(',');
                if next.is_some() && !matches!(next, Some(")") | Some("]")) {
                    self.ensure_space(out);
                }
                self.last_sig = Some(LastToken {
                    kind,
                    text: ",".to_string(),
                });
                self.pending_for = false;
            }
            _ if text == "(" => {
                self.write_indent(out);
                if needs_space_before(self.last_sig.as_ref(), text) {
                    self.ensure_space(out);
                }
                out.push('(');
                self.paren_depth = self.paren_depth.saturating_add(1);
                if self.pending_for {
                    self.for_paren_depth = Some(self.paren_depth);
                    self.pending_for = false;
                }
                self.last_sig = Some(LastToken {
                    kind,
                    text: "(".to_string(),
                });
            }
            _ if text == ")" => {
                self.write_indent(out);
                out.push(')');
                if let Some(depth) = self.for_paren_depth {
                    if depth == self.paren_depth {
                        self.for_paren_depth = None;
                    }
                }
                self.paren_depth = self.paren_depth.saturating_sub(1);
                self.last_sig = Some(LastToken {
                    kind,
                    text: ")".to_string(),
                });
                self.pending_for = false;
            }
            _ if text == "for" => {
                self.write_indent(out);
                if needs_space_between(self.last_sig.as_ref(), kind, text) {
                    self.ensure_space(out);
                }
                out.push_str(text);
                self.last_sig = Some(LastToken {
                    kind,
                    text: text.to_string(),
                });
                self.pending_for = true;
            }
            _ => {
                self.write_indent(out);
                if needs_space_between(self.last_sig.as_ref(), kind, text) {
                    self.ensure_space(out);
                }

                out.push_str(text);
                self.last_sig = Some(LastToken {
                    kind,
                    text: text.to_string(),
                });
                self.pending_for = false;
            }
        }
    }

    fn write_block_comment(&mut self, out: &mut String, text: &str) {
        // Preserve comment contents, but normalize indentation after line breaks.
        let mut lines = text.split_inclusive(['\n', '\r']);
        if let Some(first) = lines.next() {
            out.push_str(first.trim_end_matches(['\r', '\n']));
            if first.ends_with(['\n', '\r']) {
                self.ensure_newline(out);
            }
        }

        for part in lines {
            let trimmed = part.trim_end_matches(['\r', '\n']);
            self.write_indent(out);
            out.push_str(trimmed.trim_start_matches([' ', '\t']));
            if part.ends_with(['\n', '\r']) {
                self.ensure_newline(out);
            }
        }
    }
}

fn is_word_token(kind: SyntaxKind, text: &str) -> bool {
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

fn needs_space_before(last: Option<&LastToken>, next_text: &str) -> bool {
    let Some(last) = last else {
        return false;
    };

    if matches!(next_text, ")" | "]" | "}" | ";" | "," | "." | "::") {
        return false;
    }

    if matches!(last.text.as_str(), "(" | "[" | "." | "@" | "::") {
        return false;
    }

    if is_control_keyword(&last.text) && next_text == "(" {
        return true;
    }

    if next_text == "{" {
        return !matches!(last.text.as_str(), "(" | "[" | "." | "@" | "::");
    }

    is_word_token(last.kind, &last.text) && !matches!(next_text, "(")
}

fn needs_space_between(last: Option<&LastToken>, next_kind: SyntaxKind, next_text: &str) -> bool {
    let Some(last) = last else {
        return false;
    };

    if matches!(next_text, ")" | "]" | "}" | ";" | "," | "." | "::") {
        return false;
    }
    if matches!(last.text.as_str(), "(" | "[" | "." | "@" | "::") {
        return false;
    }
    if next_text == "@" {
        return true;
    }
    if is_control_keyword(&last.text) && next_text == "(" {
        return true;
    }
    if last.text == "," {
        return true;
    }

    if last.text == "]" && is_word_token(next_kind, next_text) {
        return true;
    }
    is_word_token(last.kind, &last.text) && is_word_token(next_kind, next_text)
}

fn is_control_keyword(text: &str) -> bool {
    matches!(
        text,
        "if" | "for" | "while" | "switch" | "catch" | "synchronized"
    )
}
