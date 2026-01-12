//! Java comment integration for the AST/Doc-based formatter.
//!
//! The rowan Java parser attaches trivia (including comments) to syntactic nodes as it parses,
//! which can lead to unintuitive nesting for "floating" comments. [`CommentStore`](crate::CommentStore)
//! reattaches all comments to stable token anchors (`TokenKey`).
//!
//! This module bridges [`CommentStore`] with the [`Doc`](crate::doc::Doc) model:
//! - It builds a `CommentStore` from a `SyntaxNode` and source text.
//! - It formats leading/trailing comments for a given `TokenKey` as `Doc`.
//! - It preserves blank lines between consecutive comments attached to the same anchor.
//!
//! The APIs here are intended for the upcoming AST-aware formatter; they are kept small and
//! deterministic so comment placement remains stable across formatting passes.

use nova_core::{TextRange, TextSize};
use nova_syntax::SyntaxNode;

use crate::comment_printer::{fmt_comment, FmtCtx};
use crate::doc::Doc;
use crate::{Comment, CommentKind, CommentStore, TokenKey};

/// Helper for formatting Java comments into the doc model.
///
/// In debug builds (including tests), this type asserts that all comments were consumed before it
/// is dropped. This prevents silent comment loss when integrating with the AST formatter.
pub struct JavaComments<'a> {
    source: &'a str,
    store: CommentStore,
}

impl<'a> JavaComments<'a> {
    /// Build a [`JavaComments`] store from the parsed syntax tree and original source.
    #[must_use]
    pub fn new(root: &SyntaxNode, source: &'a str) -> Self {
        let store = CommentStore::new(root, source);

        Self {
            source,
            store,
        }
    }

    /// Drain and format the leading comments that attach *before* `token`.
    #[must_use]
    pub fn take_leading_doc(&mut self, token: TokenKey, indent: usize) -> Doc<'a> {
        let comments = self.store.take_leading(token);
        if comments.is_empty() {
            return Doc::nil();
        }

        let ctx = FmtCtx::new(indent);
        self.fmt_leading_comments(&ctx, &comments)
    }

    /// Drain and format the trailing comments that attach *after* `token`.
    #[must_use]
    pub fn take_trailing_doc(&mut self, token: TokenKey, indent: usize) -> Doc<'a> {
        let comments = self.store.take_trailing(token);
        if comments.is_empty() {
            return Doc::nil();
        }

        let ctx = FmtCtx::new(indent);
        self.fmt_trailing_comments(&ctx, &comments)
    }

    /// Drain trailing comments attached to `token` but format them as *leading* comments.
    ///
    /// This is useful when a comment is lexically trailing (e.g. `{\n/*c*/ ...` or `{/*c*/ ...`)
    /// but should be rendered at the start of the following line/block rather than as an inline
    /// suffix of the preceding token.
    #[must_use]
    pub fn take_trailing_as_leading_doc(&mut self, token: TokenKey, indent: usize) -> Doc<'a> {
        let comments = self.store.take_trailing(token);
        if comments.is_empty() {
            return Doc::nil();
        }

        let ctx = FmtCtx::new(indent);
        self.fmt_leading_comments(&ctx, &comments)
    }

    pub fn assert_drained(&self) {
        self.store.assert_drained();
    }

    /// Mark any comments fully contained within `range` as consumed.
    ///
    /// This is intended for formatter fallbacks that print verbatim source slices containing
    /// comment tokens.
    pub fn consume_in_range(&mut self, range: TextRange) {
        self.store.consume_in_range(range);
    }

    fn fmt_leading_comments(
        &self,
        ctx: &FmtCtx,
        comments: &[Comment],
    ) -> Doc<'a> {
        let mut parts: Vec<Doc<'a>> = Vec::new();

        for (idx, comment) in comments.iter().enumerate() {
            parts.push(fmt_comment(ctx, comment, self.source));

            let is_last = idx + 1 == comments.len();
            if !is_last {
                let next = &comments[idx + 1];
                let line_breaks = count_line_breaks_between(
                    self.source,
                    comment.text_range.end(),
                    next.text_range.start(),
                );
                if comment.kind == CommentKind::Block && line_breaks > 0 {
                    parts.push(Doc::hardline());
                }
                if has_blank_line_between(
                    self.source,
                    comment.text_range.end(),
                    next.text_range.start(),
                ) {
                    parts.push(Doc::hardline());
                }
                continue;
            }

            // Ensure the next token/declaration cannot be glued to a standalone block comment.
            if comment.kind == CommentKind::Block {
                if comment.is_inline_with_next {
                    parts.push(Doc::text(" "));
                } else {
                    parts.push(Doc::hardline());
                }
            }
        }

        Doc::concat(parts)
    }
}

impl<'a> JavaComments<'a> {
    fn fmt_trailing_comments(
        &self,
        ctx: &FmtCtx,
        comments: &[Comment],
    ) -> Doc<'a> {
        let mut parts: Vec<Doc<'a>> = Vec::new();

        for (idx, comment) in comments.iter().enumerate() {
            match comment.kind {
                CommentKind::Line => {
                    // Trailing `//` comments are printed as line suffixes so they:
                    // - stay at the end of the current line
                    // - do not influence group fitting decisions
                    let text = comment.text(self.source).trim_end_matches(['\r', '\n']);
                    parts.push(Doc::line_suffix(Doc::concat([
                        Doc::text(" "),
                        Doc::text(text),
                    ])));
                    // `//` comments always terminate the line; force the containing group to break
                    // so any following `Doc::line()` is rendered as a newline (and flushes the
                    // suffix in the correct position).
                    parts.push(Doc::break_parent());
                }
                CommentKind::Block => {
                    // Ensure trailing block comments don't glue to the preceding token when the
                    // source omits whitespace (block comments behave like whitespace in Java, so
                    // `foo/*c*/bar` is legal). Spacing *after* the comment is handled by the
                    // surrounding formatter/token docs.
                    parts.push(Doc::text(" "));
                    parts.push(fmt_comment(ctx, comment, self.source));
                }
                _ => {
                    parts.push(fmt_comment(ctx, comment, self.source));
                }
            }

            let is_last = idx + 1 == comments.len();
            if !is_last {
                let next = &comments[idx + 1];
                let line_breaks = count_line_breaks_between(
                    self.source,
                    comment.text_range.end(),
                    next.text_range.start(),
                );
                if comment.kind == CommentKind::Block && line_breaks > 0 {
                    parts.push(Doc::hardline());
                }
                if has_blank_line_between(
                    self.source,
                    comment.text_range.end(),
                    next.text_range.start(),
                ) {
                    parts.push(Doc::hardline());
                }
                continue;
            }

        }

        Doc::concat(parts)
    }
}

impl Drop for JavaComments<'_> {
    fn drop(&mut self) {
        if cfg!(debug_assertions) && !std::thread::panicking() {
            self.store.assert_drained();
        }
    }
}

fn count_line_breaks_between(source: &str, start: TextSize, end: TextSize) -> u32 {
    let len = source.len();
    let mut start = u32::from(start) as usize;
    let mut end = u32::from(end) as usize;

    start = start.min(len);
    end = end.min(len);
    if start > end {
        std::mem::swap(&mut start, &mut end);
    }

    let bytes = &source.as_bytes()[start..end];
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

fn has_blank_line_between(source: &str, start: TextSize, end: TextSize) -> bool {
    let len = source.len();
    let mut start = u32::from(start) as usize;
    let mut end = u32::from(end) as usize;

    start = start.min(len);
    end = end.min(len);
    if start > end {
        std::mem::swap(&mut start, &mut end);
    }

    let bytes = &source.as_bytes()[start..end];
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
