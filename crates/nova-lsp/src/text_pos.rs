use std::ops::Range as ByteRange;

use lsp_types::{Position as LspPosition, Range as LspRange};
use nova_core::{LineIndex, Position as CorePosition, TextSize};

/// Helpers for converting between LSP UTF-16 positions/ranges and byte offsets.
///
/// LSP positions are expressed in UTF-16 code units, which means non-BMP
/// characters (like 😀) take two "columns". `nova_core::LineIndex` implements the
/// conversion logic and rejects positions that fall inside a surrogate pair.
#[derive(Debug, Clone)]
pub struct TextPos<'a> {
    text: &'a str,
    index: LineIndex,
}

impl<'a> TextPos<'a> {
    #[inline]
    pub fn new(text: &'a str) -> Self {
        Self {
            text,
            index: LineIndex::new(text),
        }
    }

    #[inline]
    pub fn line_index(&self) -> &LineIndex {
        &self.index
    }

    /// Convert an LSP UTF-16 position into a byte offset.
    ///
    /// Returns `None` when:
    /// - the line is out of bounds
    /// - the character is past end-of-line
    /// - the character points inside a surrogate pair
    #[inline]
    pub fn byte_offset(&self, position: LspPosition) -> Option<usize> {
        let core = CorePosition {
            line: position.line,
            character: position.character,
        };
        self.index
            .offset_of_position(self.text, core)
            .map(|offset| u32::from(offset) as usize)
    }

    /// Convert a byte offset into an LSP UTF-16 position.
    ///
    /// Returns `None` when the offset is out of bounds or is not on a UTF-8
    /// character boundary.
    #[inline]
    pub fn lsp_position(&self, offset: usize) -> Option<LspPosition> {
        if offset > self.text.len() {
            return None;
        }
        if !self.text.is_char_boundary(offset) {
            return None;
        }
        let offset_u32: u32 = offset.try_into().ok()?;
        let pos = self.index.position(self.text, TextSize::from(offset_u32));
        Some(LspPosition {
            line: pos.line,
            character: pos.character,
        })
    }

    /// Convert an LSP UTF-16 range into a byte range.
    ///
    /// Returns `None` when either endpoint is invalid or when `end < start`.
    pub fn byte_range(&self, range: LspRange) -> Option<ByteRange<usize>> {
        let start = self.byte_offset(range.start)?;
        let end = self.byte_offset(range.end)?;
        if end < start {
            return None;
        }
        let out = start..end;
        // Double-check UTF-8 boundaries so callers can slice without panicking.
        self.text.get(out.clone())?;
        Some(out)
    }
}

/// Convenience wrapper for one-off conversions.
#[inline]
pub fn byte_offset(text: &str, position: LspPosition) -> Option<usize> {
    TextPos::new(text).byte_offset(position)
}

/// Convenience wrapper for one-off conversions.
#[inline]
pub fn lsp_position(text: &str, offset: usize) -> Option<LspPosition> {
    TextPos::new(text).lsp_position(offset)
}

/// Convenience wrapper for one-off conversions.
#[inline]
pub fn byte_range(text: &str, range: LspRange) -> Option<ByteRange<usize>> {
    TextPos::new(text).byte_range(range)
}
