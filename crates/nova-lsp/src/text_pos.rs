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
        let offset_u32: u32 = match offset.try_into() {
            Ok(offset) => offset,
            Err(err) => {
                tracing::debug!(
                    target = "nova.lsp",
                    text_len = self.text.len(),
                    offset,
                    error = %err,
                    "byte offset does not fit in u32; cannot convert to LSP position"
                );
                return None;
            }
        };
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

#[derive(Debug, Clone, Copy)]
pub struct CoercedRange {
    pub start: usize,
    pub end: usize,
    pub end_was_clamped_to_eof: bool,
    pub was_reversed: bool,
}

/// Convert an LSP UTF-16 range into a byte range, with a lossy policy for the end position.
///
/// Policy:
/// - invalid start position => `None`
/// - invalid end position => clamped to end-of-file
/// - reversed ranges => normalized by swapping endpoints
pub fn coerce_range_end_to_eof(text: &str, range: LspRange) -> Option<CoercedRange> {
    let index = TextPos::new(text);
    let start = index.byte_offset(range.start)?;

    let (end, end_was_clamped_to_eof) = match index.byte_offset(range.end) {
        Some(end) => (end, false),
        None => (text.len(), true),
    };

    let (start, end, was_reversed) = if start <= end {
        (start, end, false)
    } else {
        (end, start, true)
    };

    Some(CoercedRange {
        start,
        end,
        end_was_clamped_to_eof,
        was_reversed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coerce_range_end_to_eof_rejects_invalid_start() {
        let text = "hello\nworld\n";
        let range = LspRange::new(LspPosition::new(99, 0), LspPosition::new(99, 0));
        assert!(coerce_range_end_to_eof(text, range).is_none());
    }

    #[test]
    fn coerce_range_end_to_eof_clamps_invalid_end_to_eof() {
        let text = "hello\nworld\n";
        let range = LspRange::new(LspPosition::new(0, 0), LspPosition::new(99, 0));
        let coerced = coerce_range_end_to_eof(text, range).expect("start should be valid");
        assert_eq!(coerced.start, 0);
        assert_eq!(coerced.end, text.len());
        assert!(coerced.end_was_clamped_to_eof);
        assert!(!coerced.was_reversed);
    }

    #[test]
    fn coerce_range_end_to_eof_normalizes_reversed_ranges() {
        let text = "hello\nworld\n";
        let start = LspPosition::new(1, 0);
        let end = LspPosition::new(0, 0);
        let range = LspRange::new(start, end);
        let coerced = coerce_range_end_to_eof(text, range).expect("positions should be valid");
        assert!(coerced.was_reversed);
        assert_eq!(coerced.start, 0);
        assert_eq!(coerced.end, 6); // "hello\n"
    }
}
