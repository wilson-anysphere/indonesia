use lsp_types::InsertTextFormat;
use nova_db::InMemoryFileStore;
use nova_ide::completions;
use std::path::PathBuf;

fn fixture(text_with_caret: &str) -> (InMemoryFileStore, nova_db::FileId, lsp_types::Position) {
    let caret = "<|>";
    let caret_offset = text_with_caret
        .find(caret)
        .expect("fixture must contain <|> caret marker");
    let text = text_with_caret.replace(caret, "");
    let pos = offset_to_position(&text, caret_offset);

    let mut db = InMemoryFileStore::new();
    let path = PathBuf::from("/test.java");
    let file = db.file_id_for_path(&path);
    db.set_file_text(file, text);
    (db, file, pos)
}

fn offset_to_position(text: &str, offset: usize) -> lsp_types::Position {
    let mut line = 0u32;
    let mut col = 0u32;
    for (idx, ch) in text.char_indices() {
        if idx >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 0;
        } else {
            col += 1;
        }
    }
    lsp_types::Position::new(line, col)
}

#[test]
fn completion_includes_if_snippet_template() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    <|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let if_item = items
        .iter()
        .find(|i| i.label == "if")
        .expect("expected if completion item");

    assert_eq!(if_item.insert_text_format, Some(InsertTextFormat::SNIPPET));
    assert!(
        if_item
            .insert_text
            .as_deref()
            .unwrap_or_default()
            .contains("if ("),
        "expected `if` snippet to contain `if (`; got {if_item:#?}"
    );
}

