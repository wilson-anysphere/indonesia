use lsp_types::{Position, Range};

use nova_lsp::text_pos::TextPos;

#[test]
fn utf16_position_byte_offset_round_trips_with_surrogate_pairs() {
    // 😀 is a surrogate pair in UTF-16 (2 code units, 4 bytes in UTF-8).
    let text = "a😀b\nx";
    let pos = TextPos::new(text);

    assert_eq!(pos.byte_offset(Position::new(0, 0)), Some(0));
    assert_eq!(pos.byte_offset(Position::new(0, 1)), Some(1)); // after 'a'
    assert_eq!(pos.byte_offset(Position::new(0, 3)), Some(5)); // after 😀
    assert_eq!(pos.byte_offset(Position::new(0, 4)), Some(6)); // after 'b'
    assert_eq!(pos.byte_offset(Position::new(1, 0)), Some(7)); // start of "x"

    assert_eq!(pos.lsp_position(0), Some(Position::new(0, 0)));
    assert_eq!(pos.lsp_position(1), Some(Position::new(0, 1)));
    assert_eq!(pos.lsp_position(5), Some(Position::new(0, 3)));
    assert_eq!(pos.lsp_position(6), Some(Position::new(0, 4)));
    assert_eq!(pos.lsp_position(7), Some(Position::new(1, 0)));
}

#[test]
fn utf16_positions_inside_surrogate_pairs_are_rejected() {
    let text = "a😀b\nx";
    let pos = TextPos::new(text);

    // Inside the 😀 surrogate pair (UTF-16 offset 2) is invalid.
    assert_eq!(pos.byte_offset(Position::new(0, 2)), None);

    // Offsets inside the UTF-8 encoding should also be rejected.
    assert_eq!(pos.lsp_position(2), None);
}

#[test]
fn utf16_positions_treat_crlf_as_newline_and_reject_carriage_return_columns() {
    let text = "a😀b\r\nx";
    let pos = TextPos::new(text);

    // End-of-line for "a😀b" (UTF-16 columns: a=1, 😀=2, b=1 -> 4).
    assert_eq!(pos.byte_offset(Position::new(0, 4)), Some(6));
    // Line 1 starts after the full CRLF sequence.
    assert_eq!(pos.byte_offset(Position::new(1, 0)), Some(8));

    // `character=5` would land on the `\r` code unit of the line ending; this is
    // not a valid LSP position.
    assert_eq!(pos.byte_offset(Position::new(0, 5)), None);
}

#[test]
fn utf16_range_to_byte_range_is_validated_and_safe_for_slicing() {
    let text = "a😀b\nx";
    let pos = TextPos::new(text);

    let range = Range {
        start: Position::new(0, 1),
        end: Position::new(0, 3),
    };
    let bytes = pos.byte_range(range).expect("valid range");
    assert_eq!(bytes, 1..5);
    assert_eq!(text.get(bytes.clone()), Some("😀"));
}

#[test]
fn utf16_range_inside_surrogate_pair_is_rejected() {
    let text = "a😀b\nx";
    let pos = TextPos::new(text);

    let range = Range {
        start: Position::new(0, 2),
        end: Position::new(0, 3),
    };
    assert_eq!(pos.byte_range(range), None);
}

#[test]
fn context_extraction_does_not_panic_with_unicode_selections() {
    // Use a valid Java snippet so parsing in `ContextRequest::for_java_source_range`
    // doesn't have to deal with totally arbitrary input.
    let text = "class A {\n  // 😀\n  void m() {}\n}\n";
    let pos = TextPos::new(text);

    let range = Range {
        start: Position::new(1, 5),
        end: Position::new(1, 7),
    };
    let bytes = pos.byte_range(range).expect("valid range");

    let req = nova_ai::ContextRequest::for_java_source_range(
        text,
        bytes,
        800,
        nova_ai::PrivacyMode::default(),
        /*include_doc_comments=*/ false,
    );

    assert_eq!(req.focal_code, "😀");
}
