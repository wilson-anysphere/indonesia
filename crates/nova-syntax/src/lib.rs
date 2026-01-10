//! Syntax tree and parsing primitives.
//!
//! This crate intentionally keeps the in-memory syntax representation free of
//! the original file text. Tokens store byte ranges into the current file
//! contents. Persisted syntax trees are therefore "lossless" as long as the
//! file fingerprint matches (the DB only loads cached artifacts when the
//! fingerprint matches).

use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};

/// A half-open byte range within a source file (`start..end`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TextRange {
    pub start: u32,
    pub end: u32,
}

impl TextRange {
    #[inline]
    pub fn new(start: usize, end: usize) -> Self {
        debug_assert!(start <= end);
        Self {
            start: start as u32,
            end: end as u32,
        }
    }

    #[inline]
    pub fn len(self) -> u32 {
        self.end - self.start
    }
}

/// A minimal set of syntax kinds sufficient for persistence and indexing tests.
///
/// The exact set will grow as the Java grammar is implemented.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize_repr, Deserialize_repr,
)]
#[repr(u16)]
pub enum SyntaxKind {
    CompilationUnit = 0,

    // Trivia
    Whitespace = 1,
    LineComment = 2,
    BlockComment = 3,

    // Tokens
    Identifier = 10,
    Number = 11,
    StringLiteral = 12,
    Punctuation = 13,

    // Error token / node placeholder.
    Error = 255,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GreenToken {
    pub kind: SyntaxKind,
    pub range: TextRange,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GreenChild {
    Node(Box<GreenNode>),
    Token(GreenToken),
}

impl GreenChild {
    #[inline]
    pub fn text_len(&self) -> u32 {
        match self {
            GreenChild::Node(node) => node.text_len,
            GreenChild::Token(tok) => tok.range.len(),
        }
    }
}

/// A green node is immutable and position-independent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GreenNode {
    pub kind: SyntaxKind,
    pub text_len: u32,
    pub children: Vec<GreenChild>,
}

impl GreenNode {
    pub fn new(kind: SyntaxKind, children: Vec<GreenChild>) -> Self {
        let text_len = children.iter().map(|c| c.text_len()).sum();
        Self {
            kind,
            text_len,
            children,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParseError {
    pub message: String,
    pub range: TextRange,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParseResult {
    pub root: GreenNode,
    pub errors: Vec<ParseError>,
}

impl ParseResult {
    pub fn tokens(&self) -> impl Iterator<Item = &GreenToken> {
        self.root.children.iter().filter_map(|child| match child {
            GreenChild::Token(tok) => Some(tok),
            GreenChild::Node(_) => None,
        })
    }
}

fn is_ident_start(b: u8) -> bool {
    matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'_' | b'$')
}

fn is_ident_continue(b: u8) -> bool {
    is_ident_start(b) || matches!(b, b'0'..=b'9')
}

fn is_whitespace(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

/// Parse source text into a persistent, lossless green tree and error list.
///
/// This is currently a token-level "parser" that produces a flat
/// `CompilationUnit` node. The structure is sufficient to validate persistence
/// plumbing and will be replaced by the full Java grammar.
pub fn parse(text: &str) -> ParseResult {
    let bytes = text.as_bytes();
    let mut i = 0usize;
    let mut children: Vec<GreenChild> = Vec::new();
    let mut errors = Vec::new();

    while i < bytes.len() {
        let start = i;
        let b = bytes[i];

        let kind = if is_whitespace(b) {
            i += 1;
            while i < bytes.len() && is_whitespace(bytes[i]) {
                i += 1;
            }
            SyntaxKind::Whitespace
        } else if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            SyntaxKind::LineComment
        } else if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            let mut terminated = false;
            while i + 1 < bytes.len() {
                if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    i += 2;
                    terminated = true;
                    break;
                }
                i += 1;
            }
            if !terminated {
                // Consume the rest of the file.
                i = bytes.len();
                errors.push(ParseError {
                    message: "Unterminated block comment".to_string(),
                    range: TextRange::new(start, i),
                });
            }
            SyntaxKind::BlockComment
        } else if is_ident_start(b) {
            i += 1;
            while i < bytes.len() && is_ident_continue(bytes[i]) {
                i += 1;
            }
            SyntaxKind::Identifier
        } else if matches!(b, b'0'..=b'9') {
            i += 1;
            while i < bytes.len() && matches!(bytes[i], b'0'..=b'9') {
                i += 1;
            }
            SyntaxKind::Number
        } else if b == b'"' {
            i += 1;
            let mut terminated = false;
            while i < bytes.len() {
                match bytes[i] {
                    b'\\' => {
                        // Skip escape.
                        i += 1;
                        if i < bytes.len() {
                            i += 1;
                        }
                    }
                    b'"' => {
                        i += 1;
                        terminated = true;
                        break;
                    }
                    _ => i += 1,
                }
            }
            if !terminated {
                errors.push(ParseError {
                    message: "Unterminated string literal".to_string(),
                    range: TextRange::new(start, i),
                });
            }
            SyntaxKind::StringLiteral
        } else {
            // One-byte punctuation token (keeps persistence trivial and lossless).
            i += 1;
            SyntaxKind::Punctuation
        };

        children.push(GreenChild::Token(GreenToken {
            kind,
            range: TextRange::new(start, i),
        }));
    }

    ParseResult {
        root: GreenNode {
            kind: SyntaxKind::CompilationUnit,
            text_len: text.len() as u32,
            children,
        },
        errors,
    }
}
