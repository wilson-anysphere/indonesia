use crate::doc::Doc;
use nova_syntax::{SyntaxNode, SyntaxToken};

pub(crate) fn node<'a>(source: &'a str, node: &SyntaxNode) -> Doc<'a> {
    let range = node.text_range();
    byte_range(source, u32::from(range.start()), u32::from(range.end()))
}

pub(crate) fn token<'a>(source: &'a str, token: &SyntaxToken) -> Doc<'a> {
    let range = token.text_range();
    byte_range(source, u32::from(range.start()), u32::from(range.end()))
}

pub(crate) fn byte_range<'a>(source: &'a str, start: u32, end: u32) -> Doc<'a> {
    let start = start as usize;
    let end = end as usize;
    let start = start.min(source.len());
    let end = end.min(source.len());
    let (start, end) = if start <= end { (start, end) } else { (end, start) };
    let slice = source.get(start..end).unwrap_or("");
    Doc::text(slice)
}
