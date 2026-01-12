use std::path::PathBuf;

use lsp_types::{HoverContents, Position, Range};
use nova_db::InMemoryFileStore;
use nova_ide::{completions, find_references, hover};

fn fixture_utf16(text_with_caret: &str) -> (InMemoryFileStore, nova_db::FileId, Position) {
    let caret = "<|>";
    let caret_offset = text_with_caret
        .find(caret)
        .expect("fixture must contain <|> caret marker");
    let text = text_with_caret.replace(caret, "");
    let pos = offset_to_position_utf16(&text, caret_offset);

    let mut db = InMemoryFileStore::new();
    let path = PathBuf::from("/test.java");
    let file = db.file_id_for_path(&path);
    db.set_file_text(file, text);
    (db, file, pos)
}

fn offset_to_position_utf16(text: &str, offset: usize) -> Position {
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

#[test]
fn hover_uses_utf16_positions_after_non_bmp_chars() {
    let (db, file, pos) = fixture_utf16(
        r#"
class A {
  void m() {
    String s = "🙂🙂"; int <|>x = 1; x = x + 1;
  }
}
"#,
    );

    let hover = hover(&db, file, pos).expect("expected hover at variable");
    let HoverContents::Markup(markup) = hover.contents else {
        panic!("expected markup hover contents");
    };
    assert!(
        markup.value.contains("x: int"),
        "expected hover to mention `x: int`; got {:?}",
        markup.value
    );
}

#[test]
fn completions_use_utf16_positions_after_non_bmp_chars() {
    let (db, file, pos) = fixture_utf16(
        r#"
class A {
  void m() {
    String s = "🙂🙂"; s.length<|>();
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"length"),
        "expected member completion list to contain String.length; got {labels:?}"
    );
}

#[test]
fn find_references_uses_utf16_positions_after_non_bmp_chars() {
    let fixture = r#"
class A {
  void m() {
    String s = "🙂🙂"; int <|>x = 1; x = x + 1;
  }
}
"#;
    let (db, file, pos) = fixture_utf16(fixture);

    let text = fixture.replace("<|>", "");
    let x_offsets: Vec<usize> = text.match_indices('x').map(|(idx, _)| idx).collect();
    assert_eq!(
        x_offsets.len(),
        3,
        "expected fixture to contain 3 `x` occurrences; got {x_offsets:?}"
    );

    let expected_ranges: Vec<Range> = x_offsets
        .iter()
        .map(|offset| Range {
            start: offset_to_position_utf16(&text, *offset),
            end: offset_to_position_utf16(&text, *offset + 1),
        })
        .collect();

    // `include_declaration=false` should return uses only.
    let refs = find_references(&db, file, pos, false);
    assert_eq!(
        refs.len(),
        2,
        "expected to find references excluding declaration; got {refs:?}"
    );

    let mut actual_ranges: Vec<Range> = refs.into_iter().map(|loc| loc.range).collect();
    actual_ranges.sort_by_key(|r| (r.start.line, r.start.character, r.end.line, r.end.character));

    let mut expected_uses = expected_ranges[1..].to_vec();
    expected_uses.sort_by_key(|r| (r.start.line, r.start.character, r.end.line, r.end.character));
    assert_eq!(actual_ranges, expected_uses);

    // `include_declaration=true` should include the declaration exactly once.
    let refs = find_references(&db, file, pos, true);
    assert_eq!(
        refs.len(),
        3,
        "expected to find references including declaration; got {refs:?}"
    );

    let mut actual_ranges: Vec<Range> = refs.into_iter().map(|loc| loc.range).collect();
    actual_ranges.sort_by_key(|r| (r.start.line, r.start.character, r.end.line, r.end.character));

    let mut expected = expected_ranges;
    expected.sort_by_key(|r| (r.start.line, r.start.character, r.end.line, r.end.character));
    assert_eq!(actual_ranges, expected);
}
