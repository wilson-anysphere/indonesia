use std::path::PathBuf;

use lsp_types::{HoverContents, Position};
use nova_db::RootDatabase;
use nova_ide::{completions, find_references, hover};

fn fixture_utf16(text_with_caret: &str) -> (RootDatabase, nova_db::FileId, Position) {
    let caret = "<|>";
    let caret_offset = text_with_caret
        .find(caret)
        .expect("fixture must contain <|> caret marker");
    let text = text_with_caret.replace(caret, "");
    let pos = offset_to_position_utf16(&text, caret_offset);

    let mut db = RootDatabase::new();
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
    let (db, file, pos) = fixture_utf16(
        r#"
class A {
  void m() {
    String s = "🙂🙂"; int <|>x = 1; x = x + 1;
  }
}
"#,
    );

    let refs = find_references(&db, file, pos, false);
    assert_eq!(
        refs.len(),
        3,
        "expected to find all references (decl + uses); got {refs:?}"
    );
}
