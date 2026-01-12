//! Nova Java formatter.
//!
//! The formatter is intentionally **best-effort**: it uses a token stream from `nova-syntax` to
//! avoid being confused by braces/semicolons inside comments and string literals, but it does not
//! yet rely on a full Java parser.
//!
//! Formatting is deterministic and should never panic on malformed input.

pub mod comment_printer;
pub mod comments;
mod java_comments;

use nova_core::{Position, Range, TextEdit, TextRange, TextSize};
use nova_syntax::{SyntaxKind, SyntaxTree};
use thiserror::Error;

pub use comments::{Comment, CommentKind, CommentStore, TokenKey};
pub use java_comments::JavaComments;
pub use nova_core::LineIndex;

mod formatter;
mod java_ast;
pub use java_ast::{edits_for_formatting_ast, format_java_ast};
mod java_pretty;

pub mod doc;

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
    format_member_insertion_with_newline(
        indent,
        declaration,
        needs_blank_line_after,
        NewlineStyle::Lf,
    )
}

/// Formats a single class member declaration (field/constant) for insertion using `newline`.
///
/// This is useful for refactorings so inserted code matches the file's newline style.
#[must_use]
pub fn format_member_insertion_with_newline(
    indent: &str,
    declaration: &str,
    needs_blank_line_after: bool,
    newline: NewlineStyle,
) -> String {
    let mut out = String::new();
    let newline = newline.as_str();
    out.push_str(indent);
    out.push_str(declaration.trim_end());
    out.push_str(newline);
    if needs_blank_line_after {
        out.push_str(newline);
    }
    out
}

#[derive(Debug, Clone)]
pub struct FormatConfig {
    pub indent_width: usize,
    pub indent_style: IndentStyle,
    pub max_line_length: usize,
    /// Whether to always ensure the formatted output ends with a newline.
    ///
    /// When `None`, the formatter preserves whether the input ended in a newline.
    pub insert_final_newline: Option<bool>,
    /// Whether to trim extra blank lines/newlines at the end of the document.
    ///
    /// When `None`, the formatter preserves existing behavior.
    pub trim_final_newlines: Option<bool>,
}

/// Strategy to use for full-document formatting.
///
/// Nova currently maintains multiple formatters as the implementation evolves. The intent is that
/// callers funnel full-document formatting through [`edits_for_document_formatting`] (or
/// [`edits_for_document_formatting_with_strategy`]) so the CLI + LSP stay in lockstep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FormatStrategy {
    /// Legacy, token-only formatter (`nova_syntax::parse` + [`format_java`]).
    LegacyToken,
    /// AST-aware formatter based on the rowan Java parser (`nova_syntax::parse_java`) with a
    /// token-walk formatter (`format_java_ast`).
    #[default]
    JavaTokenWalkAst,
    /// Experimental doc-based pretty-printer based on the rowan Java AST.
    ///
    /// Formatting coverage is intentionally minimal (type brace blocks + verbatim fallback),
    /// but it provides the extensible architecture needed for IntelliJ/google-java-format
    /// quality in follow-up work.
    JavaPrettyAst,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndentStyle {
    Spaces,
    Tabs,
}

impl Default for FormatConfig {
    fn default() -> Self {
        Self {
            indent_width: 4,
            indent_style: IndentStyle::Spaces,
            max_line_length: 100,
            insert_final_newline: None,
            trim_final_newlines: None,
        }
    }
}

/// A composable source formatting pipeline.
///
/// Nova's LSP integration often needs to apply multiple text-to-text transformations (e.g.
/// organize imports + formatting) but still produce minimal edits for the editor. This type
/// provides a small helper for that use case.
///
/// Each step receives the current [`SyntaxTree`] and source text and produces a new source text.
/// The pipeline reparses the updated text between steps to keep subsequent transformations in sync.
pub struct FormatPipeline<'a> {
    steps: Vec<Box<FormatPipelineStep<'a>>>,
}

type FormatPipelineStep<'a> = dyn Fn(&SyntaxTree, &str) -> String + Send + Sync + 'a;

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum NewlineStyle {
    Lf,
    CrLf,
    Cr,
}

impl NewlineStyle {
    /// Detect the newline style to use for formatter-generated output.
    ///
    /// Deterministic rule for mixed-newline documents: if the input contains any CRLF sequences,
    /// treat the file as CRLF. Otherwise fall back to LF (or bare CR if the input contains `\r`).
    pub fn detect(source: &str) -> Self {
        if source.contains("\r\n") {
            Self::CrLf
        } else if source.contains('\r') {
            Self::Cr
        } else {
            Self::Lf
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::CrLf => "\r\n",
            Self::Cr => "\r",
        }
    }
}

fn ends_with_line_break(text: &str) -> bool {
    text.ends_with('\n') || text.ends_with('\r')
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

impl<'a> Default for FormatPipeline<'a> {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Error)]
pub enum FormatError {
    #[error("invalid position")]
    InvalidPosition,
    #[error("invalid range")]
    InvalidRange,
}

/// Canonical entrypoint for full-document formatting.
///
/// This parses `source` internally and returns minimal edits that transform it into its formatted
/// representation. Both Nova's CLI and LSP should use this function so they don't diverge in
/// formatter strategy, newline detection, or final-newline policy.
pub fn edits_for_document_formatting(source: &str, config: &FormatConfig) -> Vec<TextEdit> {
    edits_for_document_formatting_with_strategy(source, config, FormatStrategy::default())
}

/// Full-document formatting with an explicit formatter strategy.
///
/// This is useful for experiments and incremental migrations, but most callers should use
/// [`edits_for_document_formatting`] so they share the workspace-wide default.
pub fn edits_for_document_formatting_with_strategy(
    source: &str,
    config: &FormatConfig,
    strategy: FormatStrategy,
) -> Vec<TextEdit> {
    match strategy {
        FormatStrategy::LegacyToken => {
            let tree = nova_syntax::parse(source);
            edits_for_formatting(&tree, source, config)
        }
        FormatStrategy::JavaTokenWalkAst => {
            let parse = nova_syntax::parse_java(source);
            edits_for_formatting_ast(&parse, source, config)
        }
        FormatStrategy::JavaPrettyAst => {
            let parse = nova_syntax::parse_java(source);
            let formatted = java_pretty::format_java_pretty(&parse, source, config);
            minimal_text_edits(source, &formatted)
        }
    }
}

/// Format an entire Java source file.
///
/// This is the legacy, token-only formatter. It is kept temporarily to support range/on-type
/// formatting while the AST-aware formatter (`format_java_ast`) lands.
pub fn format_java(tree: &SyntaxTree, source: &str, config: &FormatConfig) -> String {
    let newline = NewlineStyle::detect(source);
    let mut input_has_final_newline = ends_with_line_break(source);
    let mut formatted =
        format_java_with_indent(tree, source, config, 0, input_has_final_newline, newline);

    // Formatting is best-effort and must be idempotent, even when the lexer tokenization changes
    // across passes on malformed input (e.g. unterminated literals or comment delimiters created
    // by whitespace normalization).
    for _ in 0..8 {
        let tree = nova_syntax::parse(&formatted);
        input_has_final_newline = ends_with_line_break(&formatted);
        let next = format_java_with_indent(
            &tree,
            &formatted,
            config,
            0,
            input_has_final_newline,
            newline,
        );
        if next == formatted {
            break;
        }
        formatted = next;
    }

    formatted
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
    let newline = NewlineStyle::detect(source);
    let line_index = LineIndex::new(source);
    let range = line_index
        .text_range(source, range)
        .ok_or(FormatError::InvalidPosition)?;
    if range.is_empty() {
        return Ok(Vec::new());
    }

    let start = u32::from(range.start()) as usize;
    let end = u32::from(range.end()) as usize;
    if start > end || end > source.len() {
        return Err(FormatError::InvalidRange);
    }

    // Range formatting reparses the selected snippet in isolation. If the selection boundary cuts
    // through a string/char/comment token, the snippet is no longer valid Java and can be lexed in
    // a way that treats braces/semicolons as structural punctuation, corrupting the literal/comment
    // contents. Avoid this by producing no edits when either boundary lies *inside* such a token.
    //
    // We only check boundaries: selecting an entire literal token (including its delimiters) is
    // safe because it remains lexable as a literal in the snippet parse.
    if range_boundary_is_inside_non_code_token(tree, range.start())
        || range_boundary_is_inside_non_code_token(tree, range.end())
    {
        return Ok(Vec::new());
    }

    let indent = indent_level_at(tree, source, range.start());
    let snippet = &source[start..end];
    let snippet_tree = nova_syntax::parse(snippet);
    let keep_final_newline = ends_with_line_break(snippet);
    let config = if end == source.len() {
        config.clone()
    } else {
        FormatConfig {
            insert_final_newline: None,
            trim_final_newlines: None,
            ..config.clone()
        }
    };
    let formatted = format_java_with_indent(
        &snippet_tree,
        snippet,
        &config,
        indent,
        keep_final_newline,
        newline,
    );
    if formatted == snippet {
        return Ok(Vec::new());
    }

    let edits = minimal_text_edits(snippet, &formatted);
    let mut out = Vec::with_capacity(edits.len());
    for edit in edits {
        let start = range.start() + edit.range.start();
        let end = range.start() + edit.range.end();
        let edit_range = TextRange::new(start, end);

        if edit_range.start() < range.start() || edit_range.end() > range.end() {
            // Safety fallback: if the diff algorithm produces an out-of-range edit,
            // fall back to the original single-edit implementation.
            return Ok(vec![TextEdit::new(range, formatted)]);
        }

        out.push(TextEdit::new(edit_range, edit.replacement));
    }

    if out.is_empty() {
        // Should be unreachable because `formatted != snippet`, but be defensive and
        // preserve the previous behavior (an edit limited to `range`).
        return Ok(vec![TextEdit::new(range, formatted)]);
    }

    Ok(out)
}

fn range_boundary_is_inside_non_code_token(tree: &SyntaxTree, offset: TextSize) -> bool {
    let offset = u32::from(offset);
    for tok in tree.tokens() {
        if tok.range.end > offset {
            if tok.range.start < offset && offset < tok.range.end {
                return is_non_code_token_kind(tok.kind);
            }
            break;
        }
    }
    false
}

/// Best-effort on-type formatting.
///
/// Currently this reindents the current line for common structural triggers.
pub fn edits_for_on_type_formatting(
    tree: &SyntaxTree,
    source: &str,
    position: Position,
    ch: char,
    config: &FormatConfig,
) -> Result<Vec<TextEdit>, FormatError> {
    let line_index = LineIndex::new(source);
    let offset = line_index
        .offset_of_position(source, position)
        .ok_or(FormatError::InvalidPosition)?;

    // On-type formatting is a structural indentation adjustment. Avoid applying it inside
    // comments and string/char literals where trimming/reindenting would corrupt the literal
    // contents (text blocks, string templates, etc.).
    if is_inside_non_code_token(tree, offset) {
        return Ok(Vec::new());
    }

    let should_format = match ch {
        '}' | ';' => true,
        ')' | ',' => is_inside_argument_list(tree, source, offset),
        _ => false,
    };

    if !should_format {
        return Ok(Vec::new());
    }

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

    let new_line = format!("{}{}", indentation_for(config, indent), content);

    if new_line == line_text {
        return Ok(Vec::new());
    }

    let range = TextRange::new(line_start, line_end);
    Ok(vec![TextEdit::new(range, new_line)])
}

fn is_inside_non_code_token_at_offset(tree: &SyntaxTree, offset: TextSize) -> bool {
    let offset = u32::from(offset);
    token_kind_covering_offset(tree, offset).is_some_and(is_non_code_token_kind)
}

fn is_non_code_token_kind(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::StringLiteral
            | SyntaxKind::CharLiteral
            | SyntaxKind::TextBlock
            // String templates can be lexed into multiple tokens (`StringTemplateText`,
            // `StringTemplateExprStart`, etc.) by the full Java lexer/parser. The cache layer
            // currently coalesces templates into a single `StringLiteral`, but keep this guard
            // future-proof so snippet formatting never rewrites template payloads.
            | SyntaxKind::StringTemplateStart
            | SyntaxKind::StringTemplateText
            | SyntaxKind::StringTemplateExprStart
            | SyntaxKind::StringTemplateExprEnd
            | SyntaxKind::StringTemplateEnd
            | SyntaxKind::LineComment
            | SyntaxKind::BlockComment
            | SyntaxKind::DocComment
    )
}

fn is_inside_non_code_token(tree: &SyntaxTree, offset: TextSize) -> bool {
    // LSP on-type formatting positions are typically *after* the typed character, but keep this
    // robust across editor implementations by checking both the current offset and the previous
    // byte.
    let offset_u32 = u32::from(offset);
    is_inside_non_code_token_at_offset(tree, offset)
        || is_inside_non_code_token_at_offset(tree, TextSize::from(offset_u32.saturating_sub(1)))
}

fn token_kind_covering_offset(tree: &SyntaxTree, offset: u32) -> Option<SyntaxKind> {
    for tok in tree.tokens() {
        if tok.range.end > offset {
            if tok.range.start <= offset && offset < tok.range.end {
                return Some(tok.kind);
            }
            break;
        }
    }
    None
}

fn is_inside_argument_list(tree: &SyntaxTree, source: &str, offset: TextSize) -> bool {
    #[derive(Clone, Copy, Debug)]
    enum ParenKind {
        Argument,
        Control,
    }

    let mut stack: Vec<ParenKind> = Vec::new();
    let mut last_ident: Option<&str> = None;
    let offset_u32 = u32::from(offset);

    for token in tree.tokens() {
        if token.range.end > offset_u32 {
            break;
        }

        match token.kind {
            SyntaxKind::Whitespace
            | SyntaxKind::LineComment
            | SyntaxKind::BlockComment
            | SyntaxKind::DocComment => continue,
            SyntaxKind::Identifier => {
                last_ident = Some(token.text(source));
            }
            SyntaxKind::Punctuation => match token.text(source) {
                "(" => {
                    let kind = if last_ident.is_some_and(is_control_keyword) {
                        ParenKind::Control
                    } else {
                        ParenKind::Argument
                    };
                    stack.push(kind);
                    last_ident = None;
                }
                ")" => {
                    stack.pop();
                    last_ident = None;
                }
                _ => {
                    last_ident = None;
                }
            },
            _ => {
                last_ident = None;
            }
        }
    }

    matches!(stack.last(), Some(ParenKind::Argument))
}

fn is_control_keyword(ident: &str) -> bool {
    matches!(
        ident,
        "if" | "for" | "while" | "switch" | "catch" | "synchronized" | "try"
    )
}

/// Compute minimal edits to transform `original` into `formatted`.
///
/// This helper is intended for LSP integrations; it returns non-overlapping edits that are
/// sufficient to transform `original` into `formatted`.
pub fn minimal_text_edits(original: &str, formatted: &str) -> Vec<TextEdit> {
    if original == formatted {
        return Vec::new();
    }

    let original_lines = split_lines_inclusive(original);
    let formatted_lines = split_lines_inclusive(formatted);

    // The Myers diff algorithm below is quadratic in the worst case (and can require significant
    // memory when there are few/no matching lines). Range formatting requests are typically small,
    // so we cap the work and fall back to a single minimal replacement for very large inputs.
    const MAX_DIFF_LINES: usize = 2000;
    if original_lines.len().saturating_add(formatted_lines.len()) > MAX_DIFF_LINES {
        return minimal_text_edit(original, formatted).into_iter().collect();
    }

    if original_lines.len() == formatted_lines.len() {
        let original_offsets = line_offsets(&original_lines);
        let mut edits = Vec::new();
        for (idx, (original_line, formatted_line)) in original_lines
            .iter()
            .zip(formatted_lines.iter())
            .enumerate()
        {
            let base = TextSize::from(original_offsets[idx] as u32);
            for edit in minimal_text_edits_for_line(original_line, formatted_line) {
                let range = TextRange::new(base + edit.range.start(), base + edit.range.end());
                edits.push(TextEdit::new(range, edit.replacement));
            }
        }

        edits.sort_by_key(|edit| (edit.range.start(), edit.range.end()));
        return coalesce_adjacent_text_edits(edits);
    }

    let ops = myers_diff_ops(&original_lines, &formatted_lines);

    let mut chunks = Vec::new();
    let mut a_idx = 0usize;
    let mut b_idx = 0usize;
    let mut start_a: Option<usize> = None;
    let mut start_b: Option<usize> = None;

    for op in ops {
        match op {
            DiffOp::Equal => {
                if let (Some(sa), Some(sb)) = (start_a.take(), start_b.take()) {
                    chunks.push(DiffChunk {
                        original_start: sa,
                        original_end: a_idx,
                        formatted_start: sb,
                        formatted_end: b_idx,
                    });
                }
                a_idx += 1;
                b_idx += 1;
            }
            DiffOp::Delete => {
                if start_a.is_none() {
                    start_a = Some(a_idx);
                    start_b = Some(b_idx);
                }
                a_idx += 1;
            }
            DiffOp::Insert => {
                if start_a.is_none() {
                    start_a = Some(a_idx);
                    start_b = Some(b_idx);
                }
                b_idx += 1;
            }
        }
    }

    if let (Some(sa), Some(sb)) = (start_a, start_b) {
        chunks.push(DiffChunk {
            original_start: sa,
            original_end: a_idx,
            formatted_start: sb,
            formatted_end: b_idx,
        });
    }

    let original_offsets = line_offsets(&original_lines);
    let formatted_offsets = line_offsets(&formatted_lines);

    let mut edits = Vec::new();
    for chunk in chunks {
        let original_line_count = chunk.original_end.saturating_sub(chunk.original_start);
        let formatted_line_count = chunk.formatted_end.saturating_sub(chunk.formatted_start);

        if original_line_count == formatted_line_count
            && chunk.original_end <= original_lines.len()
            && chunk.formatted_end <= formatted_lines.len()
        {
            for idx in 0..original_line_count {
                let original_line_idx = chunk.original_start + idx;
                let formatted_line_idx = chunk.formatted_start + idx;
                let original_line = original_lines[original_line_idx];
                let formatted_line = formatted_lines[formatted_line_idx];

                let base = TextSize::from(original_offsets[original_line_idx] as u32);
                for edit in minimal_text_edits_for_line(original_line, formatted_line) {
                    let range = TextRange::new(base + edit.range.start(), base + edit.range.end());
                    edits.push(TextEdit::new(range, edit.replacement));
                }
            }
        } else {
            let original_start = original_offsets[chunk.original_start];
            let original_end = original_offsets[chunk.original_end];
            let formatted_start = formatted_offsets[chunk.formatted_start];
            let formatted_end = formatted_offsets[chunk.formatted_end];

            let original_block = &original[original_start..original_end];
            let formatted_block = &formatted[formatted_start..formatted_end];
            // Keep this low: the current Myers implementation stores the full trace (O(ND) memory),
            // so large values can become quadratic in the worst case.
            const MAX_MYERS_BLOCK_CHARS: usize = 2048;
            if let Some(block_edits) = minimal_text_edits_for_small_text(
                original_block,
                formatted_block,
                MAX_MYERS_BLOCK_CHARS,
            ) {
                let base = TextSize::from(original_start as u32);
                for edit in block_edits {
                    let range = TextRange::new(base + edit.range.start(), base + edit.range.end());
                    edits.push(TextEdit::new(range, edit.replacement));
                }
            } else if let Some(edit) = minimal_text_edit(original_block, formatted_block) {
                let base = TextSize::from(original_start as u32);
                let range = TextRange::new(base + edit.range.start(), base + edit.range.end());
                edits.push(TextEdit::new(range, edit.replacement));
            }
        }
    }

    edits.sort_by_key(|edit| (edit.range.start(), edit.range.end()));
    coalesce_adjacent_text_edits(edits)
}

fn coalesce_adjacent_text_edits(mut edits: Vec<TextEdit>) -> Vec<TextEdit> {
    // `minimal_text_edits` is intended to return non-overlapping edits suitable for LSP. In
    // practice, the diffing strategy can produce multiple insert edits at the same offset (most
    // notably around mixed newline styles, where one edit inserts a missing `\n` to form `\r\n`
    // and another edit inserts indentation at the start of the following line). Coalesce any
    // adjacent edits (including same-offset inserts) into a single replacement.
    let mut merged: Vec<TextEdit> = Vec::with_capacity(edits.len());
    for edit in edits.drain(..) {
        if let Some(last) = merged.last_mut() {
            if last.range.end() == edit.range.start() {
                last.range = TextRange::new(last.range.start(), edit.range.end());
                last.replacement.push_str(&edit.replacement);
                continue;
            }
        }
        merged.push(edit);
    }
    merged
}

fn minimal_text_edits_for_line(original: &str, formatted: &str) -> Vec<TextEdit> {
    if original == formatted {
        return Vec::new();
    }

    const MAX_MYERS_LINE_CHARS: usize = 2048;
    minimal_text_edits_for_small_text(original, formatted, MAX_MYERS_LINE_CHARS)
        .unwrap_or_else(|| minimal_text_edit(original, formatted).into_iter().collect())
}

fn minimal_text_edits_for_small_text(
    original: &str,
    formatted: &str,
    max_chars: usize,
) -> Option<Vec<TextEdit>> {
    if original == formatted {
        return Some(Vec::new());
    }

    let orig_len = original.chars().count();
    let fmt_len = formatted.chars().count();
    if orig_len.saturating_add(fmt_len) > max_chars {
        return None;
    }

    let (orig_chars, orig_offsets) = chars_with_offsets(original);
    let (fmt_chars, fmt_offsets) = chars_with_offsets(formatted);

    let ops = myers_diff_ops(&orig_chars, &fmt_chars);
    let mut chunks = Vec::new();
    let mut a_idx = 0usize;
    let mut b_idx = 0usize;
    let mut start_a: Option<usize> = None;
    let mut start_b: Option<usize> = None;

    for op in ops {
        match op {
            DiffOp::Equal => {
                if let (Some(sa), Some(sb)) = (start_a.take(), start_b.take()) {
                    chunks.push(DiffChunk {
                        original_start: sa,
                        original_end: a_idx,
                        formatted_start: sb,
                        formatted_end: b_idx,
                    });
                }
                a_idx += 1;
                b_idx += 1;
            }
            DiffOp::Delete => {
                if start_a.is_none() {
                    start_a = Some(a_idx);
                    start_b = Some(b_idx);
                }
                a_idx += 1;
            }
            DiffOp::Insert => {
                if start_a.is_none() {
                    start_a = Some(a_idx);
                    start_b = Some(b_idx);
                }
                b_idx += 1;
            }
        }
    }

    if let (Some(sa), Some(sb)) = (start_a, start_b) {
        chunks.push(DiffChunk {
            original_start: sa,
            original_end: a_idx,
            formatted_start: sb,
            formatted_end: b_idx,
        });
    }

    let mut edits = Vec::new();
    for chunk in chunks {
        let original_start = orig_offsets[chunk.original_start];
        let original_end = orig_offsets[chunk.original_end];
        let formatted_start = fmt_offsets[chunk.formatted_start];
        let formatted_end = fmt_offsets[chunk.formatted_end];
        let range = TextRange::new(
            TextSize::from(original_start as u32),
            TextSize::from(original_end as u32),
        );
        edits.push(TextEdit::new(
            range,
            formatted[formatted_start..formatted_end].to_string(),
        ));
    }

    Some(edits)
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

    Some(TextEdit::new(range, formatted[start..fmt_end].to_string()))
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiffOp {
    Equal,
    Insert,
    Delete,
}

#[derive(Clone, Copy, Debug)]
struct DiffChunk {
    original_start: usize,
    original_end: usize,
    formatted_start: usize,
    formatted_end: usize,
}

fn chars_with_offsets(text: &str) -> (Vec<char>, Vec<usize>) {
    let mut chars = Vec::new();
    let mut offsets = Vec::new();
    offsets.push(0);
    let mut offset = 0usize;
    for ch in text.chars() {
        chars.push(ch);
        offset += ch.len_utf8();
        offsets.push(offset);
    }
    (chars, offsets)
}

fn split_lines_inclusive(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut lines = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;

    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                let end = i + 1;
                lines.push(&text[start..end]);
                start = end;
                i = end;
            }
            b'\r' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                    let end = i + 2;
                    lines.push(&text[start..end]);
                    start = end;
                    i = end;
                } else {
                    let end = i + 1;
                    lines.push(&text[start..end]);
                    start = end;
                    i = end;
                }
            }
            _ => i += 1,
        }
    }

    if start < bytes.len() {
        lines.push(&text[start..]);
    }

    lines
}

fn line_offsets(lines: &[&str]) -> Vec<usize> {
    let mut offsets = Vec::with_capacity(lines.len() + 1);
    let mut offset = 0usize;
    offsets.push(offset);
    for line in lines {
        offset += line.len();
        offsets.push(offset);
    }
    offsets
}

fn myers_diff_ops<T: Eq>(a: &[T], b: &[T]) -> Vec<DiffOp> {
    if a.is_empty() && b.is_empty() {
        return Vec::new();
    }

    let n = a.len() as isize;
    let m = b.len() as isize;
    let max = (n + m) as usize;

    let mut v = vec![0isize; 2 * max + 1];
    let mut trace: Vec<Vec<isize>> = Vec::new();
    let mut found_d = 0usize;

    'outer: for d in 0..=max {
        for k in (-(d as isize)..=d as isize).step_by(2) {
            let k_idx = (k + max as isize) as usize;

            let x = if k == -(d as isize)
                || (k != d as isize
                    && v[(k - 1 + max as isize) as usize] < v[(k + 1 + max as isize) as usize])
            {
                v[(k + 1 + max as isize) as usize]
            } else {
                v[(k - 1 + max as isize) as usize] + 1
            };

            let mut x = x;
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[k_idx] = x;

            if x >= n && y >= m {
                found_d = d;
                trace.push(v.clone());
                break 'outer;
            }
        }

        trace.push(v.clone());
    }

    let mut x = n;
    let mut y = m;
    let mut ops = Vec::new();

    for d in (1..=found_d).rev() {
        let v = &trace[d - 1];
        let k = x - y;
        let prev_k = if k == -(d as isize)
            || (k != d as isize
                && v[(k - 1 + max as isize) as usize] < v[(k + 1 + max as isize) as usize])
        {
            k + 1
        } else {
            k - 1
        };

        let prev_x = v[(prev_k + max as isize) as usize];
        let prev_y = prev_x - prev_k;

        while x > prev_x && y > prev_y {
            ops.push(DiffOp::Equal);
            x -= 1;
            y -= 1;
        }

        if x == prev_x {
            ops.push(DiffOp::Insert);
            y -= 1;
        } else {
            ops.push(DiffOp::Delete);
            x -= 1;
        }
    }

    while x > 0 && y > 0 {
        ops.push(DiffOp::Equal);
        x -= 1;
        y -= 1;
    }
    while x > 0 {
        ops.push(DiffOp::Delete);
        x -= 1;
    }
    while y > 0 {
        ops.push(DiffOp::Insert);
        y -= 1;
    }

    ops.reverse();
    ops
}

fn indent_level_at(tree: &SyntaxTree, source: &str, offset: TextSize) -> usize {
    #[derive(Debug)]
    struct SwitchCtx {
        brace_depth: usize,
        in_case_body: bool,
    }

    let offset = u32::from(offset);
    let mut brace_depth: usize = 0;
    let mut pending_switch = false;
    let mut pending_case_label = false;
    let mut pending_minus = false;
    let mut switch_stack: Vec<SwitchCtx> = Vec::new();

    let mut iter = tree.tokens();
    let mut next_token = None;

    for token in iter.by_ref() {
        if token.range.end > offset {
            next_token = Some(token);
            break;
        }

        match token.kind {
            SyntaxKind::Whitespace
            | SyntaxKind::LineComment
            | SyntaxKind::BlockComment
            | SyntaxKind::DocComment => {
                pending_minus = false;
            }
            SyntaxKind::Identifier => {
                pending_minus = false;
                match token.text(source) {
                    "switch" => pending_switch = true,
                    "case" | "default" => {
                        if let Some(ctx) = switch_stack.last_mut() {
                            if brace_depth == ctx.brace_depth {
                                ctx.in_case_body = false;
                                pending_case_label = true;
                            }
                        }
                    }
                    _ => {}
                }
            }
            SyntaxKind::Punctuation => {
                let text = token.text(source);
                match text {
                    "{" => {
                        brace_depth = brace_depth.saturating_add(1);
                        if pending_switch {
                            switch_stack.push(SwitchCtx {
                                brace_depth,
                                in_case_body: false,
                            });
                            pending_switch = false;
                        }
                        pending_minus = false;
                    }
                    "->" => {
                        // Arrow switch labels (`case 1 ->`) terminate the label without entering
                        // the colon-style case body indentation.
                        pending_minus = false;
                        pending_case_label = false;
                    }
                    "}" => {
                        // If we're closing a switch block, drop its context before decrementing.
                        if switch_stack
                            .last()
                            .is_some_and(|ctx| brace_depth == ctx.brace_depth)
                        {
                            switch_stack.pop();
                            pending_case_label = false;
                        }
                        brace_depth = brace_depth.saturating_sub(1);
                        pending_minus = false;
                    }
                    ":" => {
                        if pending_case_label {
                            if let Some(ctx) = switch_stack.last_mut() {
                                if brace_depth == ctx.brace_depth {
                                    ctx.in_case_body = true;
                                }
                            }
                            pending_case_label = false;
                        }
                        pending_minus = false;
                    }
                    "-" => {
                        pending_minus = true;
                    }
                    ">" => {
                        if pending_minus {
                            // Arrow switch labels (`case 1 ->`) terminate the label without
                            // entering the colon-style case body indentation.
                            pending_minus = false;
                            pending_case_label = false;
                        } else {
                            pending_minus = false;
                        }
                    }
                    _ => {
                        pending_minus = false;
                    }
                }
            }
            _ => {
                pending_minus = false;
            }
        }
    }

    // Base indentation: braces + any active switch-case bodies.
    let case_indent = switch_stack.iter().filter(|ctx| ctx.in_case_body).count();
    let mut indent = brace_depth.saturating_add(case_indent);

    // If we're at the start of a new case/default label or the closing brace of a switch, drop
    // the case-body indentation level so range/on-type formatting aligns with the label.
    let drop_case_indent = switch_stack
        .last()
        .is_some_and(|ctx| ctx.in_case_body && brace_depth == ctx.brace_depth);

    if drop_case_indent {
        let mut sig = next_token;
        while let Some(token) = sig {
            match token.kind {
                SyntaxKind::Whitespace
                | SyntaxKind::LineComment
                | SyntaxKind::BlockComment
                | SyntaxKind::DocComment => {
                    sig = iter.next();
                }
                SyntaxKind::Identifier => {
                    if matches!(token.text(source), "case" | "default") {
                        indent = indent.saturating_sub(1);
                    }
                    break;
                }
                SyntaxKind::Punctuation if token.text(source) == "}" => {
                    indent = indent.saturating_sub(1);
                    break;
                }
                _ => break,
            }
        }
    }

    indent
}

fn format_java_with_indent(
    tree: &SyntaxTree,
    source: &str,
    config: &FormatConfig,
    initial_indent: usize,
    input_has_final_newline: bool,
    newline: NewlineStyle,
) -> String {
    formatter::format_java_with_indent(
        tree,
        source,
        config,
        initial_indent,
        input_has_final_newline,
        newline,
    )
}

fn indentation_for(config: &FormatConfig, indent_level: usize) -> String {
    match config.indent_style {
        IndentStyle::Spaces => " ".repeat(indent_level.saturating_mul(config.indent_width)),
        IndentStyle::Tabs => "\t".repeat(indent_level),
    }
}
