use lsp_types::{Position, Range};

use nova_core::{LineIndex, TextSize};

use nova_types::Span;

/// Cached conversions between UTF-8 byte offsets (Nova internal spans) and LSP
/// UTF-16 positions.
///
/// Building a [`LineIndex`] is O(n) in the text length, but then individual
/// conversions are O(log(lines) + line_len) instead of scanning the entire file
/// from the start.
#[derive(Debug, Clone)]
pub(crate) struct TextIndex<'a> {
    text: &'a str,
    index: LineIndex,
}

impl<'a> TextIndex<'a> {
    #[must_use]
    pub(crate) fn new(text: &'a str) -> Self {
        Self {
            text,
            index: LineIndex::new(text),
        }
    }

    #[must_use]
    pub(crate) fn position_to_offset(&self, position: Position) -> Option<usize> {
        position_to_offset_with_index(&self.index, self.text, position)
    }

    #[must_use]
    pub(crate) fn offset_to_position(&self, offset: usize) -> Position {
        offset_to_position_with_index(&self.index, self.text, offset)
    }

    #[must_use]
    pub(crate) fn span_to_lsp_range(&self, span: Span) -> Range {
        span_to_lsp_range_with_index(&self.index, self.text, span)
    }

    #[must_use]
    #[allow(dead_code)]
    pub(crate) fn line_index(&self) -> &LineIndex {
        &self.index
    }
}

#[must_use]
pub(crate) fn position_to_offset_with_index(
    index: &LineIndex,
    text: &str,
    position: Position,
) -> Option<usize> {
    let core_pos = nova_core::text::Position::new(position.line, position.character);
    let offset = index.offset_of_position(text, core_pos)?;
    Some(u32::from(offset) as usize)
}

#[must_use]
pub(crate) fn offset_to_position_with_index(
    index: &LineIndex,
    text: &str,
    offset: usize,
) -> Position {
    let offset = offset.min(text.len());
    let offset_u32 = match u32::try_from(offset) {
        Ok(offset) => offset,
        Err(_) => u32::MAX,
    };
    let pos = index.position(text, TextSize::from(offset_u32));
    Position::new(pos.line, pos.character)
}

#[must_use]
pub(crate) fn span_to_lsp_range_with_index(index: &LineIndex, text: &str, span: Span) -> Range {
    Range {
        start: offset_to_position_with_index(index, text, span.start),
        end: offset_to_position_with_index(index, text, span.end),
    }
}

#[must_use]
#[allow(dead_code)]
pub fn position_to_offset(text: &str, position: Position) -> Option<usize> {
    let mut line: u32 = 0;
    let mut col_utf16: u32 = 0;
    let mut offset: usize = 0;

    for ch in text.chars() {
        if line == position.line && col_utf16 == position.character {
            return Some(offset);
        }

        offset += ch.len_utf8();
        if ch == '\n' {
            line += 1;
            col_utf16 = 0;
        } else {
            col_utf16 += ch.len_utf16() as u32;
        }
    }

    if line == position.line && col_utf16 == position.character {
        Some(offset)
    } else {
        None
    }
}

#[must_use]
#[allow(dead_code)]
pub fn offset_to_position(text: &str, offset: usize) -> Position {
    let mut line: u32 = 0;
    let mut col_utf16: u32 = 0;
    let mut cur: usize = 0;

    for ch in text.chars() {
        if cur >= offset {
            break;
        }
        cur += ch.len_utf8();
        if ch == '\n' {
            line += 1;
            col_utf16 = 0;
        } else {
            col_utf16 += ch.len_utf16() as u32;
        }
    }

    Position {
        line,
        character: col_utf16,
    }
}

#[must_use]
#[allow(dead_code)]
pub fn span_to_lsp_range(text: &str, span: Span) -> Range {
    Range {
        start: offset_to_position(text, span.start),
        end: offset_to_position(text, span.end),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_index_matches_slow_conversions_on_large_text() {
        let mut text = String::new();
        for i in 0..2000 {
            // Include some non-BMP characters to exercise surrogate pair handling.
            if i % 17 == 0 {
                text.push('😀');
            }
            text.push_str(&format!("line {i} αβγ\n"));
        }
        text.push_str("last 😀 line");

        let fast = TextIndex::new(&text);

        // Pick a bunch of UTF-8 byte offsets that are guaranteed to be at
        // character boundaries (via `char_indices`).
        let mut offsets = Vec::new();
        offsets.push(0);
        for (idx, (byte_idx, _)) in text.char_indices().enumerate() {
            if idx % 137 == 0 {
                offsets.push(byte_idx);
            }
        }
        offsets.push(text.len());

        for offset in offsets {
            let slow_pos = offset_to_position(&text, offset);
            let fast_pos = fast.offset_to_position(offset);
            assert_eq!(
                fast_pos, slow_pos,
                "offset_to_position mismatch at offset {offset}"
            );

            let slow_off = position_to_offset(&text, slow_pos)
                .expect("slow position from offset should map back to offset");
            let fast_off = fast
                .position_to_offset(fast_pos)
                .expect("fast position from offset should map back to offset");

            assert_eq!(
                fast_off, slow_off,
                "position_to_offset mismatch at position {fast_pos:?}"
            );
        }
    }
}
