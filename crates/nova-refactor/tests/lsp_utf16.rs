use lsp_types::Uri;
use nova_core::{LineIndex, TextSize};
use nova_refactor::{
    position_to_offset_utf16, workspace_edit_to_lsp, FileId, TextDatabase, WorkspaceEdit, WorkspaceTextEdit,
    WorkspaceTextRange,
};
use pretty_assertions::assert_eq;

#[test]
fn workspace_edit_to_lsp_uses_utf16_for_surrogate_pairs() {
    // 😀 is a surrogate pair in UTF-16 (2 code units, 4 bytes in UTF-8).
    let uri: Uri = "file:///Test.java".parse().unwrap();
    let file = FileId::new(uri.to_string());
    let text = "a😀b";

    let db = TextDatabase::new([(file.clone(), text.to_string())]);
    let edit = WorkspaceEdit::new(vec![WorkspaceTextEdit::replace(
        file.clone(),
        WorkspaceTextRange::new(5, 6), // replace `b`
        "B",
    )]);

    let lsp = workspace_edit_to_lsp(&db, &edit).unwrap();
    let changes = lsp.changes.unwrap();
    let edits = changes.get(&uri).unwrap();
    assert_eq!(edits.len(), 1);

    let index = LineIndex::new(text);
    let expected_start = index.position(text, TextSize::from(5u32));
    let expected_end = index.position(text, TextSize::from(6u32));

    assert_eq!(expected_start.line, 0);
    assert_eq!(expected_start.character, 3);
    assert_eq!(expected_end.line, 0);
    assert_eq!(expected_end.character, 4);

    let range = &edits[0].range;
    assert_eq!(range.start.line, expected_start.line);
    assert_eq!(range.start.character, expected_start.character);
    assert_eq!(range.end.line, expected_end.line);
    assert_eq!(range.end.character, expected_end.character);
}

#[test]
fn workspace_edit_to_lsp_does_not_treat_character_as_utf8_bytes() {
    // é is 2 bytes in UTF-8 but 1 code unit in UTF-16.
    let uri: Uri = "file:///Test.java".parse().unwrap();
    let file = FileId::new(uri.to_string());
    let text = "aéb";

    let db = TextDatabase::new([(file.clone(), text.to_string())]);
    let edit = WorkspaceEdit::new(vec![WorkspaceTextEdit::replace(
        file.clone(),
        WorkspaceTextRange::new(3, 4), // replace `b`
        "B",
    )]);

    let lsp = workspace_edit_to_lsp(&db, &edit).unwrap();
    let changes = lsp.changes.unwrap();
    let edits = changes.get(&uri).unwrap();
    assert_eq!(edits.len(), 1);

    let index = LineIndex::new(text);
    let expected_start = index.position(text, TextSize::from(3u32));
    let expected_end = index.position(text, TextSize::from(4u32));

    assert_eq!(expected_start.line, 0);
    assert_eq!(expected_start.character, 2);
    assert_eq!(expected_end.line, 0);
    assert_eq!(expected_end.character, 3);

    let range = &edits[0].range;
    assert_eq!(range.start.line, expected_start.line);
    assert_eq!(range.start.character, expected_start.character);
    assert_eq!(range.end.line, expected_end.line);
    assert_eq!(range.end.character, expected_end.character);
}

#[test]
fn workspace_edit_to_lsp_uses_line_index_crlf_semantics() {
    // Ensure CRLF is treated as a single newline boundary (line ends at `\r`,
    // next line starts after `\n`) and that `\r` is not counted into the UTF-16
    // column offsets.
    //
    // Include a non-BMP char before the edited range to validate UTF-16 counting.
    let uri: Uri = "file:///Test.java".parse().unwrap();
    let file = FileId::new(uri.to_string());
    let text = "a😀b\r\nc";

    // Target the `\n` byte inside the CRLF sequence.
    let offset = text.find('\n').expect("expected CRLF newline");

    let db = TextDatabase::new([(file.clone(), text.to_string())]);
    let edit = WorkspaceEdit::new(vec![WorkspaceTextEdit::replace(
        file.clone(),
        WorkspaceTextRange::new(offset, offset),
        "X",
    )]);

    let lsp = workspace_edit_to_lsp(&db, &edit).unwrap();
    let changes = lsp.changes.unwrap();
    let edits = changes.get(&uri).unwrap();
    assert_eq!(edits.len(), 1);

    let index = LineIndex::new(text);
    let expected = index.position(text, TextSize::from(offset as u32));

    let range = &edits[0].range;
    assert_eq!(range.start.line, expected.line);
    assert_eq!(range.start.character, expected.character);
    assert_eq!(range.end.line, expected.line);
    assert_eq!(range.end.character, expected.character);
}

#[test]
fn position_to_offset_utf16_matches_line_index_and_rejects_surrogates() {
    // 😀 is a surrogate pair in UTF-16 (2 code units, 4 bytes in UTF-8).
    let text = "a😀b\nx";
    let index = LineIndex::new(text);

    // Valid positions round-trip.
    for (line, character) in [(0, 0), (0, 1), (0, 3), (0, 4), (1, 0)] {
        let expected = index
            .offset_of_position(text, nova_core::Position::new(line, character))
            .map(|offset| u32::from(offset) as usize);
        let actual = position_to_offset_utf16(text, lsp_types::Position { line, character });
        assert_eq!(actual, expected);
    }

    // Inside the surrogate pair is invalid.
    assert_eq!(
        position_to_offset_utf16(
            text,
            lsp_types::Position {
                line: 0,
                character: 2
            }
        ),
        None
    );
}
