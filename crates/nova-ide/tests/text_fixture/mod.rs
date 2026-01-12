//! Shared helpers for nova-ide integration test fixtures.
//!
//! LSP `Position.character` is defined in terms of UTF-16 code units. Many tests work with byte
//! offsets (e.g. `<|>` caret markers) and need a correct conversion.

use nova_core::{LineIndex, Position as CorePosition, TextSize};

#[allow(dead_code)]
pub const CARET: &str = "<|>";

/// Convert a UTF-8 byte offset into an LSP [`lsp_types::Position`] using UTF-16 `character`
/// semantics.
pub fn offset_to_position(text: &str, offset: usize) -> lsp_types::Position {
    let mut offset = offset.min(text.len());
    // `LineIndex::position` slices the source text, so ensure we never provide an offset that
    // lands inside a UTF-8 code point.
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    let offset_u32 = u32::try_from(offset).unwrap_or(u32::MAX);

    let index = LineIndex::new(text);
    let pos = index.position(text, TextSize::from(offset_u32));
    lsp_types::Position::new(pos.line, pos.character)
}

/// Convert an LSP [`lsp_types::Position`] (UTF-16 code units) into a UTF-8 byte offset.
pub fn position_to_offset(text: &str, position: lsp_types::Position) -> Option<usize> {
    let index = LineIndex::new(text);
    index
        .offset_of_position(text, CorePosition::new(position.line, position.character))
        .map(|o| u32::from(o) as usize)
}
