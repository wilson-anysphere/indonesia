//! Nova Java formatter.
//!
//! The formatter is intentionally **best-effort**: it uses a token stream from `nova-syntax` to
//! avoid being confused by braces/semicolons inside comments and string literals, but it does not
//! yet rely on a full Java parser.
//!
//! Formatting is deterministic and should never panic on malformed input.

use nova_core::{Position, Range, TextEdit, TextRange, TextSize};
use nova_syntax::{SyntaxKind, SyntaxTree};
use thiserror::Error;

pub use nova_core::LineIndex;

/// Indents each non-empty line in `block` with `indent`.
#[must_use]
pub fn indent_block(block: &str, indent: &str) -> String {
    let mut out = String::with_capacity(block.len() + indent.len() * 4);
    for (idx, line) in block.split_inclusive('\n').enumerate() {
        let line_stripped = line.strip_suffix('\n').unwrap_or(line);
        if !line_stripped.trim().is_empty() {
            out.push_str(indent);
        } else if idx == 0 && line_stripped.is_empty() {
            // Preserve leading empty line without indentation.
        }
        out.push_str(line_stripped);
        if line.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// Removes common leading indentation from all non-empty lines in `block`.
#[must_use]
pub fn dedent_block(block: &str) -> String {
    let lines: Vec<&str> = block.lines().collect();
    let min_indent = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.chars().take_while(|c| c.is_whitespace()).count())
        .min()
        .unwrap_or(0);

    let mut out = String::with_capacity(block.len());
    for (idx, line) in lines.iter().enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        if line.trim().is_empty() {
            out.push_str(line);
        } else {
            let mut byte_idx = 0usize;
            let mut removed = 0usize;
            for (i, ch) in line.char_indices() {
                if removed >= min_indent {
                    break;
                }
                if ch.is_whitespace() {
                    byte_idx = i + ch.len_utf8();
                    removed += 1;
                } else {
                    break;
                }
            }
            out.push_str(&line[byte_idx..]);
        }
    }
    out
}

/// Formats a single class member declaration (field/constant) for insertion.
///
/// `indent` is the indentation used for class members (typically 4 spaces more
/// than the class declaration indentation).
///
/// `needs_blank_line_after` controls whether we add an extra blank line after
/// the declaration (useful when inserting a field before a method).
#[must_use]
pub fn format_member_insertion(
    indent: &str,
    declaration: &str,
    needs_blank_line_after: bool,
) -> String {
    let mut out = String::new();
    out.push_str(indent);
    out.push_str(declaration.trim_end());
    out.push('\n');
    if needs_blank_line_after {
        out.push('\n');
    }
    out
}

#[derive(Debug, Clone)]
pub struct FormatConfig {
    pub indent_width: usize,
    pub max_line_length: usize,
}

impl Default for FormatConfig {
    fn default() -> Self {
        Self {
            indent_width: 4,
            max_line_length: 100,
        }
    }
}

/// A composable source formatting pipeline.
///
/// Nova's LSP integration often needs to apply multiple text-to-text transformations (e.g.
/// organize imports + formatting) but still produce a single minimal edit for the editor. This
/// type provides a small helper for that use case.
///
/// Each step receives the current [`SyntaxTree`] and source text and produces a new source text.
/// The pipeline reparses the updated text between steps to keep subsequent transformations in sync.
pub struct FormatPipeline<'a> {
    steps: Vec<Box<dyn Fn(&SyntaxTree, &str) -> String + Send + Sync + 'a>>,
}

impl<'a> FormatPipeline<'a> {
    pub fn new() -> Self {
        Self { steps: Vec::new() }
    }

    pub fn push_step<F>(&mut self, step: F)
    where
        F: Fn(&SyntaxTree, &str) -> String + Send + Sync + 'a,
    {
        self.steps.push(Box::new(step));
    }

    pub fn run(&self, mut tree: SyntaxTree, mut text: String) -> (SyntaxTree, String) {
        for step in &self.steps {
            text = step(&tree, &text);
            tree = nova_syntax::parse(&text);
        }
        (tree, text)
    }

    pub fn run_and_diff(&self, tree: &SyntaxTree, source: &str) -> Vec<TextEdit> {
        let (_tree, text) = self.run(tree.clone(), source.to_string());
        minimal_text_edits(source, &text)
    }
}

#[derive(Debug, Error)]
pub enum FormatError {
    #[error("invalid position")]
    InvalidPosition,
    #[error("invalid range")]
    InvalidRange,
}

/// Format an entire Java source file.
pub fn format_java(tree: &SyntaxTree, source: &str, config: &FormatConfig) -> String {
    let ensure_final_newline = source.ends_with('\n') || source.ends_with("\r\n");
    format_java_with_indent(tree, source, config, 0, ensure_final_newline)
}

/// Return minimal edits that transform `source` into its formatted representation.
pub fn edits_for_formatting(
    tree: &SyntaxTree,
    source: &str,
    config: &FormatConfig,
) -> Vec<TextEdit> {
    let formatted = format_java(tree, source, config);
    minimal_text_edits(source, &formatted)
}

/// Format only the selected range, preserving the text outside the range.
///
/// This is designed for LSP `textDocument/rangeFormatting`. The returned edits are guaranteed to
/// be restricted to `range`.
pub fn edits_for_range_formatting(
    tree: &SyntaxTree,
    source: &str,
    range: Range,
    config: &FormatConfig,
) -> Result<Vec<TextEdit>, FormatError> {
    let line_index = LineIndex::new(source);
    let range = line_index
        .text_range(source, range)
        .ok_or(FormatError::InvalidPosition)?;
    let start = u32::from(range.start()) as usize;
    let end = u32::from(range.end()) as usize;
    if start > end || end > source.len() {
        return Err(FormatError::InvalidRange);
    }

    let indent = indent_level_at(tree, source, range.start());
    let snippet = &source[start..end];
    let snippet_tree = nova_syntax::parse(snippet);
    let keep_final_newline = snippet.ends_with('\n') || snippet.ends_with("\r\n");
    let formatted =
        format_java_with_indent(&snippet_tree, snippet, config, indent, keep_final_newline);
    if formatted == snippet {
        return Ok(Vec::new());
    }
    Ok(vec![TextEdit::new(range, formatted)])
}

/// Best-effort on-type formatting.
///
/// Currently this reindents the current line for `}` and `;` triggers.
pub fn edits_for_on_type_formatting(
    tree: &SyntaxTree,
    source: &str,
    position: Position,
    ch: char,
    config: &FormatConfig,
) -> Result<Vec<TextEdit>, FormatError> {
    if ch != '}' && ch != ';' {
        return Ok(Vec::new());
    }

    let line_index = LineIndex::new(source);
    let line_start = line_index
        .line_start(position.line)
        .ok_or(FormatError::InvalidPosition)?;
    let line_end = line_index
        .line_end(position.line)
        .ok_or(FormatError::InvalidPosition)?;

    let start_usize = u32::from(line_start) as usize;
    let end_usize = u32::from(line_end) as usize;
    let line_text = &source[start_usize..end_usize];
    let content = line_text.trim();

    if content.is_empty() {
        return Ok(Vec::new());
    }

    let mut indent = indent_level_at(tree, source, line_start);
    if content.starts_with('}') && indent > 0 {
        indent -= 1;
    }

    let new_line = format!(
        "{}{}",
        " ".repeat(indent.saturating_mul(config.indent_width)),
        content
    );

    if new_line == line_text {
        return Ok(Vec::new());
    }

    let range = TextRange::new(line_start, line_end);
    Ok(vec![TextEdit::new(range, new_line)])
}

/// Compute minimal edits to transform `original` into `formatted`.
///
/// This helper is intended for LSP integrations; it returns either zero edits (already formatted)
/// or a single edit that replaces the changed span.
pub fn minimal_text_edits(original: &str, formatted: &str) -> Vec<TextEdit> {
    minimal_text_edit(original, formatted).into_iter().collect()
}

fn minimal_text_edit(original: &str, formatted: &str) -> Option<TextEdit> {
    if original == formatted {
        return None;
    }

    let start = common_prefix_len(original, formatted);
    let (orig_end, fmt_end) = common_suffix_ends(original, formatted, start);
    let range = TextRange::new(
        TextSize::from(start as u32),
        TextSize::from(orig_end as u32),
    );

    Some(TextEdit::new(
        range,
        formatted[start..fmt_end].to_string(),
    ))
}

fn common_prefix_len(a: &str, b: &str) -> usize {
    let mut len = 0usize;
    for (ac, bc) in a.chars().zip(b.chars()) {
        if ac != bc {
            break;
        }
        len += ac.len_utf8();
    }
    len
}

fn common_suffix_ends(a: &str, b: &str, prefix: usize) -> (usize, usize) {
    let mut a_end = a.len();
    let mut b_end = b.len();

    let mut a_rev = a.char_indices().rev();
    let mut b_rev = b.char_indices().rev();

    while let (Some((a_idx, a_ch)), Some((b_idx, b_ch))) = (a_rev.next(), b_rev.next()) {
        if a_idx < prefix || b_idx < prefix {
            break;
        }
        if a_ch != b_ch {
            break;
        }
        a_end = a_idx;
        b_end = b_idx;
    }

    (a_end, b_end)
}

fn indent_level_at(tree: &SyntaxTree, source: &str, offset: TextSize) -> usize {
    let mut indent: usize = 0;
    let offset = u32::from(offset);
    for token in tree.tokens() {
        if token.range.end > offset {
            break;
        }
        if token.kind != SyntaxKind::Punctuation {
            continue;
        }
        match token.text(source) {
            "{" => indent = indent.saturating_add(1),
            "}" => indent = indent.saturating_sub(1),
            _ => {}
        }
    }
    indent
}

fn format_java_with_indent(
    tree: &SyntaxTree,
    source: &str,
    config: &FormatConfig,
    initial_indent: usize,
    ensure_final_newline: bool,
) -> String {
    let mut out = String::new();
    let mut state = FormatState::new(config, initial_indent);
    let tokens: Vec<nova_syntax::GreenToken> = tree.tokens().cloned().collect();

    let mut idx = 0usize;
    while idx < tokens.len() {
        let token = &tokens[idx];
        idx += 1;

        if token.kind == SyntaxKind::Whitespace {
            if count_line_breaks(token.text(source)) >= 2 {
                state.pending_blank_line = true;
            }
            continue;
        }

        if state.pending_blank_line {
            state.ensure_blank_line(&mut out);
            state.pending_blank_line = false;
        }

        let text = token.text(source);
        let next = next_significant(&tokens, idx).map(|t| t.text(source));

        state.write_token(&mut out, token.kind, text, next);
    }

    if ensure_final_newline {
        state.ensure_newline(&mut out);
    } else {
        // Trim trailing whitespace/newlines while keeping the output stable. For range formatting
        // we deliberately avoid appending an extra newline because the edit range may already be
        // bounded by an existing line break in the original document.
        while matches!(out.chars().last(), Some(' ' | '\t' | '\n')) {
            out.pop();
        }
    }

    out
}

fn next_significant<'a>(
    tokens: &'a [nova_syntax::GreenToken],
    mut idx: usize,
) -> Option<&'a nova_syntax::GreenToken> {
    while idx < tokens.len() {
        if tokens[idx].kind != SyntaxKind::Whitespace {
            return Some(&tokens[idx]);
        }
        idx += 1;
    }
    None
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
struct FormatState<'a> {
    config: &'a FormatConfig,
    indent_level: usize,
    at_line_start: bool,
    pending_blank_line: bool,
    paren_depth: usize,
    for_paren_depth: Option<usize>,
    pending_for: bool,
    last_sig: Option<LastToken>,
}

#[derive(Debug, Clone)]
struct LastToken {
    kind: SyntaxKind,
    text: String,
}

impl<'a> FormatState<'a> {
    fn new(config: &'a FormatConfig, initial_indent: usize) -> Self {
        Self {
            config,
            indent_level: initial_indent,
            at_line_start: true,
            pending_blank_line: false,
            paren_depth: 0,
            for_paren_depth: None,
            pending_for: false,
            last_sig: None,
        }
    }

    fn ensure_newline(&mut self, out: &mut String) {
        while matches!(out.chars().last(), Some(' ' | '\t')) {
            out.pop();
        }
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        self.at_line_start = true;
    }

    fn ensure_blank_line(&mut self, out: &mut String) {
        if out.is_empty() {
            return;
        }
        self.ensure_newline(out);
        if !out.ends_with("\n\n") {
            out.push('\n');
        }
        self.at_line_start = true;
    }

    fn write_indent(&mut self, out: &mut String) {
        if !self.at_line_start {
            return;
        }
        out.push_str(&" ".repeat(self.indent_level.saturating_mul(self.config.indent_width)));
        self.at_line_start = false;
    }

    fn ensure_space(&mut self, out: &mut String) {
        if self.at_line_start {
            return;
        }
        if out.is_empty() || matches!(out.chars().last(), Some(' ' | '\n' | '\t')) {
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
            SyntaxKind::Punctuation if text == "{" => {
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
            SyntaxKind::Punctuation if text == "}" => {
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
            SyntaxKind::Punctuation if text == ";" => {
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
            SyntaxKind::Punctuation if text == "," => {
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
            SyntaxKind::Punctuation if text == "(" => {
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
            SyntaxKind::Punctuation if text == ")" => {
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
            SyntaxKind::Identifier if text == "for" => {
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

fn is_word(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::Identifier
            | SyntaxKind::Number
            | SyntaxKind::StringLiteral
            | SyntaxKind::CharLiteral
    )
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

    if last.kind == SyntaxKind::Identifier && is_control_keyword(&last.text) && next_text == "(" {
        return true;
    }

    if next_text == "{" {
        return !matches!(last.text.as_str(), "(" | "[" | "." | "@" | "::");
    }

    is_word(last.kind) && !matches!(next_text, "(")
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
    if last.kind == SyntaxKind::Identifier && is_control_keyword(&last.text) && next_text == "(" {
        return true;
    }
    if last.kind == SyntaxKind::Punctuation && last.text == "," {
        return true;
    }

    if last.kind == SyntaxKind::Punctuation && last.text == "]" && is_word(next_kind) {
        return true;
    }
    is_word(last.kind) && is_word(next_kind)
}

fn is_control_keyword(text: &str) -> bool {
    matches!(
        text,
        "if" | "for" | "while" | "switch" | "catch" | "synchronized"
    )
}
