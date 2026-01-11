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
fn completion_includes_lombok_getters() {
    let (db, file, pos) = fixture(
        r#"
import lombok.Getter;

class Foo {
  @Getter int x;
  @Getter boolean active;
}

class Use {
  void m() {
    Foo f = new Foo();
    f.<|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();

    assert!(
        labels.contains(&"getX"),
        "expected completion list to contain Lombok getX; got {labels:?}"
    );
    assert!(
        labels.contains(&"isActive"),
        "expected completion list to contain Lombok isActive; got {labels:?}"
    );
}
