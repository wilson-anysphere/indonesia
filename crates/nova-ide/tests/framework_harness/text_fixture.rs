//! Shared helpers for nova-ide integration test fixtures.
//!
//! LSP `Position.character` is defined in terms of UTF-16 code units. Many tests work with byte
//! offsets (e.g. `<|>` caret markers) and need a correct conversion.

use nova_core::{LineIndex, TextSize};

pub const CARET: &str = "<|>";

/// Convert a UTF-8 byte offset into an LSP [`lsp_types::Position`] using UTF-16 `character`
/// semantics.
pub fn offset_to_position(text: &str, offset: usize) -> lsp_types::Position {
    let offset = offset.min(text.len());
    let offset_u32 = u32::try_from(offset).unwrap_or(u32::MAX);

    let index = LineIndex::new(text);
    let pos = index.position(text, TextSize::from(offset_u32));
    lsp_types::Position::new(pos.line, pos.character)
}
