//! Token-based "item tree" used for Nova's early-cutoff demo.
//!
//! This module exists to support the `nova-db` early-cutoff tests and cache
//! plumbing while the real Java grammar and HIR item tree are still under active
//! development.
//!
//! The implementation is intentionally tiny: it scans the flat token stream and
//! recognizes `class`/`interface`/`enum`/`record` followed by an identifier.
//! Long-term this will be replaced by a proper parser-derived summary, but the
//! persisted shape here is useful for exercising incremental recomputation and
//! warm-start persistence.

use nova_syntax::{ParseResult, SyntaxKind, TextRange};
use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};

struct RkyvTextRange;

impl rkyv::with::ArchiveWith<TextRange> for RkyvTextRange {
    type Archived = [u32; 2];
    type Resolver = ();

    unsafe fn resolve_with(
        field: &TextRange,
        _pos: usize,
        _resolver: Self::Resolver,
        out: *mut Self::Archived,
    ) {
        out.write([field.start, field.end]);
    }
}

impl<S> rkyv::with::SerializeWith<TextRange, S> for RkyvTextRange
where
    S: rkyv::ser::Serializer + ?Sized,
{
    fn serialize_with(_field: &TextRange, _serializer: &mut S) -> Result<Self::Resolver, S::Error> {
        Ok(())
    }
}

impl<D> rkyv::with::DeserializeWith<[u32; 2], TextRange, D> for RkyvTextRange
where
    D: rkyv::Fallible + ?Sized,
{
    fn deserialize_with(field: &[u32; 2], _deserializer: &mut D) -> Result<TextRange, D::Error> {
        Ok(TextRange {
            start: field[0],
            end: field[1],
        })
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize_repr, Deserialize_repr,
    rkyv::Archive, rkyv::Serialize, rkyv::Deserialize,
)]
#[archive(check_bytes)]
#[repr(u8)]
pub enum TokenItemKind {
    Class = 0,
    Interface = 1,
    Enum = 2,
    Record = 3,
}

#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
#[archive(check_bytes)]
pub struct TokenItem {
    pub kind: TokenItemKind,
    pub name: String,
    #[with(RkyvTextRange)]
    pub name_range: TextRange,
}

#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
#[archive(check_bytes)]
pub struct TokenItemTree {
    pub items: Vec<TokenItem>,
}

impl TokenItemTree {
    #[must_use]
    pub fn empty() -> Self {
        Self { items: Vec::new() }
    }
}

/// Optional per-file symbol summary used by indexing.
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
#[archive(check_bytes)]
pub struct TokenSymbolSummary {
    pub names: Vec<String>,
}

impl TokenSymbolSummary {
    #[must_use]
    pub fn from_item_tree(item_tree: &TokenItemTree) -> Self {
        Self {
            names: item_tree.items.iter().map(|it| it.name.clone()).collect(),
        }
    }
}

fn token_text(text: &str, range: TextRange) -> &str {
    let start = range.start as usize;
    let end = range.end as usize;
    &text[start..end]
}

fn is_trivia(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::Whitespace
            | SyntaxKind::LineComment
            | SyntaxKind::BlockComment
            | SyntaxKind::DocComment
    )
}

/// Build a per-file [`TokenItemTree`] from a flat token stream.
///
/// This is *not* a real Java parser. It's a small recognizer used to create a
/// persisted per-file summary for the early-cutoff demo.
#[must_use]
pub fn token_item_tree(parse: &ParseResult, text: &str) -> TokenItemTree {
    let tokens: Vec<_> = parse.tokens().collect();
    let mut items = Vec::new();
    let mut i = 0usize;

    while i < tokens.len() {
        let tok = tokens[i];
        if tok.kind != SyntaxKind::Identifier {
            i += 1;
            continue;
        }

        let kw = token_text(text, tok.range);
        let (kind, is_decl) = match kw {
            "class" => (TokenItemKind::Class, true),
            "interface" => (TokenItemKind::Interface, true),
            "enum" => (TokenItemKind::Enum, true),
            "record" => (TokenItemKind::Record, true),
            _ => (TokenItemKind::Class, false),
        };

        if !is_decl {
            i += 1;
            continue;
        }

        // Find the next non-trivia token.
        let mut j = i + 1;
        while j < tokens.len() && is_trivia(tokens[j].kind) {
            j += 1;
        }

        if j < tokens.len() && tokens[j].kind == SyntaxKind::Identifier {
            let name_tok = tokens[j];
            items.push(TokenItem {
                kind,
                name: token_text(text, name_tok.range).to_string(),
                name_range: name_tok.range,
            });
        }

        i = j + 1;
    }

    TokenItemTree { items }
}
