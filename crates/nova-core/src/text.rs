//! Text model primitives: sizes, ranges, positions, and conversions.

pub use text_size::{TextRange, TextSize};

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub struct LineCol {
    pub line: u32,
    pub col: u32,
}

/// LSP-compatible position (UTF-16 code units).
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
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

/// LSP-compatible range (UTF-16 code units).
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

impl Range {
    #[inline]
    pub const fn new(start: Position, end: Position) -> Self {
        Self { start, end }
    }

    #[inline]
    pub const fn point(pos: Position) -> Self {
        Self {
            start: pos,
            end: pos,
        }
    }
}

/// Pre-computed line start offsets for a particular text snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LineIndex {
    line_starts: Vec<TextSize>,
    line_ends: Vec<TextSize>,
    text_len: TextSize,
}

impl LineIndex {
    pub fn new(text: &str) -> Self {
        let bytes = text.as_bytes();
        let mut line_starts = Vec::with_capacity(128);
        let mut line_ends = Vec::with_capacity(128);
        line_starts.push(TextSize::from(0));

        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'\n' => {
                    line_ends.push(TextSize::from(i as u32));
                    line_starts.push(TextSize::from((i + 1) as u32));
                    i += 1;
                }
                b'\r' => {
                    if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                        line_ends.push(TextSize::from(i as u32));
                        line_starts.push(TextSize::from((i + 2) as u32));
                        i += 2;
                    } else {
                        line_ends.push(TextSize::from(i as u32));
                        line_starts.push(TextSize::from((i + 1) as u32));
                        i += 1;
                    }
                }
                _ => i += 1,
            }
        }

        line_ends.push(TextSize::from(text.len() as u32));

        Self {
            line_starts,
            line_ends,
            text_len: TextSize::from(text.len() as u32),
        }
    }

    #[inline]
    pub fn text_len(&self) -> TextSize {
        self.text_len
    }

    #[inline]
    pub fn line_count(&self) -> u32 {
        self.line_starts.len() as u32
    }

    #[inline]
    pub fn line_start(&self, line: u32) -> Option<TextSize> {
        self.line_starts.get(line as usize).copied()
    }

    #[inline]
    pub fn line_end(&self, line: u32) -> Option<TextSize> {
        self.line_ends.get(line as usize).copied()
    }

    fn line_index(&self, offset: TextSize) -> usize {
        // Clamp offsets that point past the end; callers may pass `text_len`
        // when referring to EOF.
        let offset = offset.min(self.text_len);
        match self.line_starts.binary_search(&offset) {
            Ok(line) => line,
            Err(insert) => insert.saturating_sub(1),
        }
    }

    /// Convert a byte offset to a UTF-8 (byte) line/column pair.
    pub fn line_col(&self, offset: TextSize) -> LineCol {
        let offset = offset.min(self.text_len);
        let line = self.line_index(offset);
        let line_start = self.line_starts[line];
        let line_end = self.line_ends[line];
        let col = offset.min(line_end) - line_start;
        LineCol {
            line: line as u32,
            col: u32::from(col),
        }
    }

    /// Convert a UTF-8 (byte) line/column pair to a byte offset.
    pub fn offset(&self, line_col: LineCol) -> Option<TextSize> {
        let start = self.line_start(line_col.line)?;
        let end = self.line_end(line_col.line)?;
        let offset = start + TextSize::from(line_col.col);
        if offset > end {
            return None;
        }
        Some(offset)
    }

    /// Convert a byte offset to an LSP-compatible UTF-16 position.
    ///
    /// `text` must be the same snapshot used to construct this [`LineIndex`].
    pub fn position(&self, text: &str, offset: TextSize) -> Position {
        debug_assert_eq!(TextSize::from(text.len() as u32), self.text_len);
        let offset = offset.min(self.text_len);
        let line = self.line_index(offset);
        let line_start = self.line_starts[line];
        let line_end = self.line_ends[line];
        let offset = offset.min(line_end);
        let line_start_usize = u32::from(line_start) as usize;
        let offset_usize = u32::from(offset) as usize;
        let utf16_col: u32 = text[line_start_usize..offset_usize]
            .chars()
            .map(|c| c.len_utf16() as u32)
            .sum();

        Position {
            line: line as u32,
            character: utf16_col,
        }
    }

    /// Convert an LSP-compatible UTF-16 position into a byte offset.
    ///
    /// Returns `None` if:
    /// - `line` is out of bounds
    /// - the UTF-16 `character` is past the end of the line
    /// - `character` points inside a surrogate pair
    pub fn offset_of_position(&self, text: &str, position: Position) -> Option<TextSize> {
        debug_assert_eq!(TextSize::from(text.len() as u32), self.text_len);
        let line_start = self.line_start(position.line)?;
        let line_end_excl_newline = self.line_end(position.line)?;

        let line_start_usize = u32::from(line_start) as usize;
        let line_end_usize = u32::from(line_end_excl_newline) as usize;
        let line_text = &text[line_start_usize..line_end_usize];

        let mut utf16 = 0u32;
        if position.character == 0 {
            return Some(line_start);
        }

        for (byte_idx, ch) in line_text.char_indices() {
            let ch_utf16 = ch.len_utf16() as u32;

            if utf16 == position.character {
                return Some(line_start + TextSize::from(byte_idx as u32));
            }

            if utf16 + ch_utf16 > position.character {
                return None;
            }

            utf16 += ch_utf16;
        }

        if utf16 == position.character {
            Some(line_end_excl_newline)
        } else {
            None
        }
    }

    /// Convert a byte range to an LSP-compatible range using UTF-16 positions.
    pub fn range(&self, text: &str, range: TextRange) -> Range {
        Range {
            start: self.position(text, range.start()),
            end: self.position(text, range.end()),
        }
    }

    /// Convert an LSP-compatible range into a byte range.
    pub fn text_range(&self, text: &str, range: Range) -> Option<TextRange> {
        let start = self.offset_of_position(text, range.start)?;
        let end = self.offset_of_position(text, range.end)?;
        Some(TextRange::new(start, end))
    }
}

#[cfg(feature = "lsp")]
mod lsp_compat {
    use super::{Position, Range};

    impl From<Position> for lsp_types::Position {
        fn from(value: Position) -> Self {
            lsp_types::Position {
                line: value.line,
                character: value.character,
            }
        }
    }

    impl From<lsp_types::Position> for Position {
        fn from(value: lsp_types::Position) -> Self {
            Position {
                line: value.line,
                character: value.character,
            }
        }
    }

    impl From<Range> for lsp_types::Range {
        fn from(value: Range) -> Self {
            lsp_types::Range {
                start: value.start.into(),
                end: value.end.into(),
            }
        }
    }

    impl From<lsp_types::Range> for Range {
        fn from(value: lsp_types::Range) -> Self {
            Range {
                start: value.start.into(),
                end: value.end.into(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_surrogate_pair_conversions() {
        // 😀 is a surrogate pair in UTF-16 (2 code units, 4 bytes in UTF-8).
        let text = "a😀b\nx";
        let index = LineIndex::new(text);

        // Offsets (UTF-8 bytes) to UTF-16 positions.
        assert_eq!(
            index.position(text, TextSize::from(0)),
            Position {
                line: 0,
                character: 0
            }
        );
        assert_eq!(
            index.position(text, TextSize::from(1)),
            Position {
                line: 0,
                character: 1
            }
        );
        assert_eq!(
            index.position(text, TextSize::from(5)),
            Position {
                line: 0,
                character: 3
            }
        );
        assert_eq!(
            index.position(text, TextSize::from(6)),
            Position {
                line: 0,
                character: 4
            }
        );
        assert_eq!(
            index.position(text, TextSize::from(7)),
            Position {
                line: 1,
                character: 0
            }
        );

        // UTF-16 positions to offsets.
        assert_eq!(
            index.offset_of_position(
                text,
                Position {
                    line: 0,
                    character: 0
                }
            ),
            Some(TextSize::from(0))
        );
        assert_eq!(
            index.offset_of_position(
                text,
                Position {
                    line: 0,
                    character: 1
                }
            ),
            Some(TextSize::from(1))
        );
        assert_eq!(
            index.offset_of_position(
                text,
                Position {
                    line: 0,
                    character: 3
                }
            ),
            Some(TextSize::from(5))
        );
        assert_eq!(
            index.offset_of_position(
                text,
                Position {
                    line: 0,
                    character: 4
                }
            ),
            Some(TextSize::from(6))
        );
        assert_eq!(
            index.offset_of_position(
                text,
                Position {
                    line: 1,
                    character: 0
                }
            ),
            Some(TextSize::from(7))
        );

        // Inside the surrogate pair is invalid.
        assert_eq!(
            index.offset_of_position(
                text,
                Position {
                    line: 0,
                    character: 2
                }
            ),
            None
        );
    }
}
