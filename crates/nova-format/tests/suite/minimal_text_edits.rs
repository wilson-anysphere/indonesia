use nova_core::apply_text_edits;
use nova_format::minimal_text_edits;

#[test]
fn minimal_text_edits_coalesces_same_offset_inserts_across_mixed_newlines() {
    // This input intentionally mixes bare CR and CRLF. When formatting normalizes the newline
    // style, `minimal_text_edits` must not return multiple inserts at the same offset (which would
    // violate LSP's non-overlapping edit requirement).
    let original = "a\rb";
    let formatted = "a\r\n b";

    let edits = minimal_text_edits(original, formatted);
    let applied = apply_text_edits(original, &edits).unwrap();
    assert_eq!(applied, formatted);
}
