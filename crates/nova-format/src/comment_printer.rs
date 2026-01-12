//! Comment rendering helpers for Nova's rowan-based Java formatter.
//!
//! This module is deliberately independent of any rowan tree APIs. It operates purely on
//! [`Comment`](crate::Comment) metadata (produced by [`CommentStore`](crate::CommentStore)) plus the
//! original source text, and returns a [`Doc`](crate::doc::Doc) that can be stitched into a larger
//! pretty-printing document.
//!
//! ## Trailing line comments
//!
//! When printing a `//` comment as a *trailing* comment, the caller is expected to insert exactly
//! one space between the preceding token and the comment. This module never inserts leading
//! whitespace for line comments.

use crate::{doc::Doc, Comment, CommentKind};

/// Rendering context for a single comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FmtCtx {
    /// The indentation (in spaces) that should be applied after *hard* line breaks inside the
    /// comment.
    ///
    /// The formatter typically uses this to align the comment body with the surrounding code when
    /// a comment spans multiple lines.
    pub indent: usize,
}

impl FmtCtx {
    #[inline]
    pub const fn new(indent: usize) -> Self {
        Self { indent }
    }
}

/// Returns `true` if `text` contains any line break sequence (`\n`, `\r`, or `\r\n`).
#[inline]
pub fn comment_contains_line_break(text: &str) -> bool {
    text.as_bytes().iter().any(|b| matches!(b, b'\n' | b'\r'))
}

/// Counts the number of line break sequences in `text`.
///
/// This is CRLF-aware: `\r\n` counts as a single line break.
#[must_use]
pub fn count_line_breaks(text: &str) -> u32 {
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

/// Format a single comment as a [`Doc`].
///
/// The returned doc is nested by `ctx.indent` so that any hard line breaks introduced while
/// rendering the comment re-indent to the caller-specified indentation.
#[must_use]
pub fn fmt_comment<'a>(ctx: &FmtCtx, comment: &Comment, src: &'a str) -> Doc<'a> {
    let text = comment.text(src);
    let doc = match comment.kind {
        CommentKind::Line => fmt_line_comment(text),
        CommentKind::Block => fmt_block_comment(text),
        CommentKind::Doc => fmt_doc_comment(text),
    };
    doc.nest(ctx.indent)
}

fn fmt_line_comment<'a>(text: &'a str) -> Doc<'a> {
    let text = text.trim_end_matches(['\r', '\n']);
    Doc::concat([Doc::text(text), Doc::hardline()])
}

fn fmt_block_comment<'a>(text: &'a str) -> Doc<'a> {
    if !comment_contains_line_break(text) {
        return Doc::text(text);
    }

    let lines = split_lines(text);
    let common = common_indent(lines.iter().skip(1).map(|l| l.text));

    let mut parts: Vec<Doc<'a>> = Vec::with_capacity(lines.len() * 2);
    parts.push(Doc::text(lines[0].text));

    for idx in 1..lines.len() {
        if lines[idx - 1].has_line_break {
            parts.push(Doc::hardline());
            let line = trim_indent(lines[idx].text, common);
            parts.push(Doc::text(line));
        } else {
            parts.push(Doc::text(lines[idx].text));
        }
    }

    Doc::concat(parts)
}

fn fmt_doc_comment<'a>(text: &'a str) -> Doc<'a> {
    let text = text.trim_end_matches(['\r', '\n']);

    if !comment_contains_line_break(text) {
        // JavaDoc must be its own block: ensure it cannot share a line with the next token.
        return Doc::concat([Doc::text(text), Doc::hardline()]);
    }

    let lines = split_lines(text);
    let common = common_indent(lines.iter().skip(1).map(|l| l.text));

    let mut parts: Vec<Doc<'a>> = Vec::with_capacity(lines.len() * 3 + 1);
    parts.push(Doc::text(lines[0].text));

    for idx in 1..lines.len() {
        if !lines[idx - 1].has_line_break {
            parts.push(Doc::text(lines[idx].text));
            continue;
        }

        parts.push(Doc::hardline());

        let raw = trim_indent(lines[idx].text, common);
        if raw.trim().is_empty() {
            continue;
        }

        let trimmed = raw.trim_start_matches([' ', '\t']);
        if trimmed.starts_with("*/") {
            // Closing delimiter aligns with the opening indentation.
            parts.push(Doc::text(trimmed));
        } else if let Some(rest) = trimmed.strip_prefix('*') {
            // Normalize interior lines to ` <indent> * ...` style.
            parts.push(Doc::concat([Doc::text(" *"), Doc::text(rest)]));
        } else {
            parts.push(Doc::text(raw));
        }
    }

    // Ensure the doc comment cannot share a line with the following declaration.
    parts.push(Doc::hardline());
    Doc::concat(parts)
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Line<'a> {
    pub(crate) text: &'a str,
    pub(crate) has_line_break: bool,
}

pub(crate) fn split_lines(text: &str) -> Vec<Line<'_>> {
    let bytes = text.as_bytes();
    let mut lines = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;

    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                lines.push(Line {
                    text: &text[start..i],
                    has_line_break: true,
                });
                i += 1;
                start = i;
            }
            b'\r' => {
                lines.push(Line {
                    text: &text[start..i],
                    has_line_break: true,
                });
                i += 1;
                if i < bytes.len() && bytes[i] == b'\n' {
                    i += 1;
                }
                start = i;
            }
            _ => i += 1,
        }
    }

    lines.push(Line {
        text: &text[start..],
        has_line_break: false,
    });

    lines
}

pub(crate) fn common_indent<'a>(lines: impl Iterator<Item = &'a str>) -> usize {
    let mut min: Option<usize> = None;

    for line in lines {
        if line.trim().is_empty() {
            continue;
        }

        let indent = line
            .chars()
            .take_while(|ch| matches!(ch, ' ' | '\t'))
            .count();
        min = Some(min.map_or(indent, |min| min.min(indent)));
    }

    min.unwrap_or(0)
}

pub(crate) fn trim_indent(line: &str, indent: usize) -> &str {
    if indent == 0 {
        return line;
    }

    let mut removed = 0usize;
    let mut byte_idx = 0usize;
    for (i, ch) in line.char_indices() {
        if removed >= indent {
            break;
        }
        match ch {
            ' ' | '\t' => {
                removed += 1;
                byte_idx = i + ch.len_utf8();
            }
            _ => break,
        }
    }

    &line[byte_idx..]
}
