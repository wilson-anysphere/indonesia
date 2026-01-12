pub use crate::framework_harness::{offset_to_position, position_to_offset, CARET};

#[test]
fn utf16_position_round_trip_for_non_bmp_char() {
    // LSP `Position.character` is defined in terms of UTF-16 code units. Ensure our helpers
    // preserve byte offsets across a non-BMP character.
    let text = "a🙂b\n";

    let offset = text
        .find("🙂")
        .expect("fixture should contain non-BMP char");
    let pos = offset_to_position(text, offset);
    assert_eq!(position_to_offset(text, pos), Some(offset));

    // Touch the caret constant (used by many fixture helpers).
    assert_eq!(CARET, "<|>");
}

#[test]
fn offset_to_position_clamps_mid_codepoint_offsets() {
    // Some callers compute offsets in bytes (e.g. substring searches) and could accidentally land
    // inside a multi-byte UTF-8 codepoint. Our helper should be robust and clamp down to the
    // previous char boundary rather than panicking.
    let text = "a🙂b\n";

    let emoji_start = text
        .find("🙂")
        .expect("fixture should contain non-BMP char");
    let pos_at_char_boundary = offset_to_position(text, emoji_start);
    let pos_inside_codepoint = offset_to_position(text, emoji_start + 1);
    assert_eq!(pos_inside_codepoint, pos_at_char_boundary);

    // LSP positions that point into a surrogate pair should be rejected.
    let inside_surrogate_pair = lsp_types::Position::new(
        pos_at_char_boundary.line,
        pos_at_char_boundary.character + 1,
    );
    assert_eq!(position_to_offset(text, inside_surrogate_pair), None);
}
