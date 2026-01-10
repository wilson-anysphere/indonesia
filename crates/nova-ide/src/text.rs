use lsp_types::{Position, Range};

use nova_types::Span;

#[must_use]
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
pub fn span_to_lsp_range(text: &str, span: Span) -> Range {
    Range {
        start: offset_to_position(text, span.start),
        end: offset_to_position(text, span.end),
    }
}
