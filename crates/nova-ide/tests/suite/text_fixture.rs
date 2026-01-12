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
