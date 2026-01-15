use lsp_types::Position as LspPosition;

pub(super) fn position_to_offset_utf16(text: &str, position: LspPosition) -> Option<usize> {
    nova_lsp::text_pos::byte_offset(text, position)
}

pub(super) fn offset_to_position_utf16(text: &str, offset: usize) -> LspPosition {
    let mut clamped = offset.min(text.len());
    while clamped > 0 && !text.is_char_boundary(clamped) {
        clamped -= 1;
    }
    nova_lsp::text_pos::lsp_position(text, clamped).unwrap_or_else(|| LspPosition::new(0, 0))
}

pub(super) fn ident_range_at(text: &str, offset: usize) -> Option<(usize, usize)> {
    fn is_ident_continue(b: u8) -> bool {
        (b as char).is_ascii_alphanumeric() || b == b'_' || b == b'$'
    }

    let bytes = text.as_bytes();
    if offset > bytes.len() {
        return None;
    }

    let mut start = offset.min(bytes.len());
    while start > 0 && is_ident_continue(bytes[start - 1]) {
        start -= 1;
    }

    let mut end = offset.min(bytes.len());
    while end < bytes.len() && is_ident_continue(bytes[end]) {
        end += 1;
    }

    if start == end { None } else { Some((start, end)) }
}

