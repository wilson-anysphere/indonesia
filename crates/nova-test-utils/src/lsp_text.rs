use lsp_types::Position;

/// Converts a UTF-8 byte offset into an LSP UTF-16 position.
///
/// If `offset` is out of bounds, it is clamped to `text.len()`. If it lands on a non-UTF8 boundary,
/// it is rounded down to the previous UTF-8 boundary.
#[must_use]
pub fn offset_to_position(text: &str, offset: usize) -> Position {
    let mut offset = offset.min(text.len());
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }

    let mut line: u32 = 0;
    let mut col_utf16: u32 = 0;
    let mut idx: usize = 0;

    for ch in text.chars() {
        if idx >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            col_utf16 = 0;
        } else {
            col_utf16 += ch.len_utf16() as u32;
        }
        idx += ch.len_utf8();
    }

    Position {
        line,
        character: col_utf16,
    }
}

/// Converts an LSP UTF-16 position into a UTF-8 byte offset.
///
/// Returns `None` if the requested position is not representable in `text` (e.g. the column falls
/// inside a surrogate pair or past the end of the line).
#[must_use]
pub fn position_to_offset(text: &str, pos: Position) -> Option<usize> {
    let mut line: u32 = 0;
    let mut col_utf16: u32 = 0;
    let mut idx: usize = 0;

    for ch in text.chars() {
        if line == pos.line && col_utf16 == pos.character {
            return Some(idx);
        }

        if ch == '\n' {
            if line == pos.line {
                if col_utf16 == pos.character {
                    return Some(idx);
                }
                return None;
            }
            line += 1;
            col_utf16 = 0;
            idx += 1;
            continue;
        }

        if line == pos.line {
            col_utf16 += ch.len_utf16() as u32;
            if col_utf16 > pos.character {
                return None;
            }
        }
        idx += ch.len_utf8();
    }

    if line == pos.line && col_utf16 == pos.character {
        Some(idx)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_offsets_at_char_boundaries() {
        let text = "a\n😃b\nαβ";
        for offset in [0, 1, 2, "a\n".len(), "a\n😃".len(), text.len()] {
            let pos = offset_to_position(text, offset);
            let back =
                position_to_offset(text, pos).expect("position from offset should roundtrip");
            assert_eq!(back, offset);
        }
    }

    #[test]
    fn clamps_offsets_to_valid_utf8_boundary() {
        let text = "😃";
        // Offset 1 is in the middle of the emoji's UTF-8 encoding.
        let pos = offset_to_position(text, 1);
        assert_eq!(pos, Position::new(0, 0));
    }
}
