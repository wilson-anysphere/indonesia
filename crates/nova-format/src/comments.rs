//! Comment / trivia infrastructure for the upcoming rowan-based Java formatter.
//!
//! The rowan parser (`nova_syntax::parse_java`) currently attaches trivia to the *current* node as
//! it parses. This means comment tokens can end up nested in unintuitive places (e.g. comments
//! between class members often live under the next member's `Modifiers` node).
//!
//! Formatting cannot rely on tree-local trivia. Instead, [`CommentStore`] walks *all* tokens in
//! lexical order and attaches comments to stable anchors (usually the adjacent non-trivia token).

use std::collections::HashMap;

use nova_core::{TextRange, TextSize};
use nova_syntax::{SyntaxKind, SyntaxNode, SyntaxToken};

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

fn is_significant_token(kind: SyntaxKind) -> bool {
    !kind.is_trivia() && !is_synthetic_missing(kind)
}

/// Stable identifier for a token used as a comment anchor.
///
/// We intentionally key by text range rather than holding on to rowan token handles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TokenKey {
    pub start: u32,
    pub end: u32,
}

impl TokenKey {
    #[must_use]
    pub fn text_range(self) -> TextRange {
        TextRange::new(TextSize::from(self.start), TextSize::from(self.end))
    }
}

impl From<TextRange> for TokenKey {
    fn from(range: TextRange) -> Self {
        Self {
            start: u32::from(range.start()),
            end: u32::from(range.end()),
        }
    }
}

impl From<&SyntaxToken> for TokenKey {
    fn from(token: &SyntaxToken) -> Self {
        Self::from(token.text_range())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentKind {
    Line,
    Block,
    Doc,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    pub kind: CommentKind,
    pub text_range: TextRange,

    pub is_inline_with_prev: bool,
    pub is_inline_with_next: bool,
    pub blank_line_before: bool,
    pub blank_line_after: bool,
    pub force_own_line_after: bool,
}

impl Comment {
    #[must_use]
    pub fn text<'a>(&self, source: &'a str) -> &'a str {
        let start = u32::from(self.text_range.start()) as usize;
        let end = u32::from(self.text_range.end()) as usize;
        source.get(start..end).unwrap_or("")
    }
}

#[derive(Debug, Default)]
pub struct CommentStore {
    leading: HashMap<TokenKey, Vec<Comment>>,
    trailing: HashMap<TokenKey, Vec<Comment>>,
}

impl CommentStore {
    /// Extract all comment tokens from `root` and attach them to stable token anchors.
    ///
    /// This is intentionally independent of trivia nesting in the rowan tree: all tokens are
    /// processed in lexical order and comments are attached based on raw source gaps between
    /// significant tokens.
    #[must_use]
    pub fn new(root: &SyntaxNode, source: &str) -> Self {
        #[derive(Clone, Copy)]
        struct TokenInfo {
            kind: SyntaxKind,
            range: TextRange,
        }

        let tokens: Vec<TokenInfo> = root
            .descendants_with_tokens()
            .filter_map(|e| e.into_token())
            .map(|t| TokenInfo {
                kind: t.kind(),
                range: t.text_range(),
            })
            .collect();

        let mut prev_sig_before = vec![None; tokens.len()];
        let mut last_sig: Option<usize> = None;
        for (idx, tok) in tokens.iter().enumerate() {
            prev_sig_before[idx] = last_sig;
            if is_significant_token(tok.kind) {
                last_sig = Some(idx);
            }
        }

        let mut next_sig_after = vec![None; tokens.len()];
        let mut next_sig: Option<usize> = None;
        for idx in (0..tokens.len()).rev() {
            next_sig_after[idx] = next_sig;
            if is_significant_token(tokens[idx].kind) {
                next_sig = Some(idx);
            }
        }

        let eof_key = TokenKey {
            start: source.len() as u32,
            end: source.len() as u32,
        };

        let mut store = Self::default();

        for (idx, tok) in tokens.iter().enumerate() {
            let (kind, force_own_line_after) = match tok.kind {
                SyntaxKind::LineComment => (CommentKind::Line, false),
                SyntaxKind::BlockComment => (CommentKind::Block, false),
                SyntaxKind::DocComment => (CommentKind::Doc, true),
                _ => continue,
            };

            let prev_sig = prev_sig_before[idx];
            let next_sig = next_sig_after[idx];

            let prev_end = prev_sig
                .map(|i| tokens[i].range.end())
                .unwrap_or_else(|| TextSize::from(0));
            let next_start = next_sig
                .map(|i| tokens[i].range.start())
                .unwrap_or_else(|| TextSize::from(source.len() as u32));

            let line_breaks_before = count_line_breaks_between(source, prev_end, tok.range.start());
            let line_breaks_after = count_line_breaks_between(source, tok.range.end(), next_start);
            let blank_line_before = has_blank_line_between(source, prev_end, tok.range.start());
            let blank_line_after = has_blank_line_between(source, tok.range.end(), next_start);

            let is_inline_with_prev = prev_sig.is_some() && line_breaks_before == 0;
            let is_inline_with_next = next_sig.is_some() && line_breaks_after == 0;

            let comment = Comment {
                kind,
                text_range: tok.range,
                is_inline_with_prev,
                is_inline_with_next,
                blank_line_before,
                blank_line_after,
                force_own_line_after,
            };

            let (anchor, is_trailing) = match kind {
                CommentKind::Doc => (next_sig, false),
                _ if is_inline_with_prev => (prev_sig, true),
                _ => (next_sig, false),
            };

            let key = anchor.map_or(eof_key, |i| TokenKey::from(tokens[i].range));

            if is_trailing {
                store.trailing.entry(key).or_default().push(comment);
            } else {
                store.leading.entry(key).or_default().push(comment);
            }
        }

        store
    }

    /// Drain comments that attach *before* `token`.
    #[must_use]
    pub fn take_leading(&mut self, token: TokenKey) -> Vec<Comment> {
        self.leading.remove(&token).unwrap_or_else(Vec::new)
    }

    /// Drain comments that attach *after* `token`.
    #[must_use]
    pub fn take_trailing(&mut self, token: TokenKey) -> Vec<Comment> {
        self.trailing.remove(&token).unwrap_or_else(Vec::new)
    }

    #[must_use]
    pub fn peek_leading(&self, token: TokenKey) -> &[Comment] {
        self.leading.get(&token).map_or(&[], |c| c.as_slice())
    }

    #[must_use]
    pub fn peek_trailing(&self, token: TokenKey) -> &[Comment] {
        self.trailing.get(&token).map_or(&[], |c| c.as_slice())
    }

    /// Assert that all comments were consumed via [`take_leading`] / [`take_trailing`].
    pub fn assert_drained(&self) {
        assert!(
            self.leading.is_empty() && self.trailing.is_empty(),
            "unconsumed comments: leading={:?}, trailing={:?}",
            self.leading.keys().collect::<Vec<_>>(),
            self.trailing.keys().collect::<Vec<_>>(),
        );
    }

    /// Mark any comments whose text range is fully contained within `range` as consumed.
    ///
    /// This is useful when formatting falls back to emitting verbatim source slices that already
    /// contain comment tokens (so they are not "dropped", but also not explicitly drained via
    /// [`take_leading`] / [`take_trailing`]).
    pub fn consume_in_range(&mut self, range: TextRange) {
        consume_map_in_range(&mut self.leading, range);
        consume_map_in_range(&mut self.trailing, range);
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

        // After a line break, a "blank line" means we encounter another line break with only
        // horizontal whitespace (` ` / `\t`) in between.
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

fn consume_map_in_range(map: &mut HashMap<TokenKey, Vec<Comment>>, range: TextRange) {
    map.retain(|_, comments| {
        comments.retain(|comment| !range_fully_contains(range, comment.text_range));
        !comments.is_empty()
    });
}

fn range_fully_contains(outer: TextRange, inner: TextRange) -> bool {
    inner.start() >= outer.start() && inner.end() <= outer.end()
}
