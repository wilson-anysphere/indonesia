//! Core shared types for Nova.
//!
//! This crate is intentionally small and dependency-free.

/// A position in a text document expressed as (line, UTF-16 code unit offset).
///
/// This matches the Language Server Protocol definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

impl Position {
    #[inline]
    pub const fn new(line: u32, character: u32) -> Self {
        Self { line, character }
    }
}

/// A half-open range in a text document expressed with LSP positions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

impl Range {
    #[inline]
    pub const fn new(start: Position, end: Position) -> Self {
        Self { start, end }
    }
}

/// A textual edit described by a range replacement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEdit {
    pub range: Range,
    pub new_text: String,
}

impl TextEdit {
    #[inline]
    pub fn new(range: Range, new_text: impl Into<String>) -> Self {
        Self {
            range,
            new_text: new_text.into(),
        }
    }
}

