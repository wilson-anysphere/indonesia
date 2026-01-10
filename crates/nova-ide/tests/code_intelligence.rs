use nova_db::RootDatabase;
use nova_ide::{completions, file_diagnostics, goto_definition};
use nova_types::Severity;
use std::path::PathBuf;

fn fixture(text_with_caret: &str) -> (RootDatabase, nova_db::FileId, lsp_types::Position) {
    let caret = "<|>";
    let caret_offset = text_with_caret
        .find(caret)
        .expect("fixture must contain <|> caret marker");
    let mut text = text_with_caret.to_string();
    text = text.replace(caret, "");

    let pos = offset_to_position(&text, caret_offset);

    let mut db = RootDatabase::new();
    let path = PathBuf::from("/test.java");
    let file = db.file_id_for_path(&path);
    db.set_file_text(file, text);
    (db, file, pos)
}

fn fixture_file(text: &str) -> (RootDatabase, nova_db::FileId) {
    let mut db = RootDatabase::new();
    let path = PathBuf::from("/test.java");
    let file = db.file_id_for_path(&path);
    db.set_file_text(file, text.to_string());
    (db, file)
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
fn completion_includes_string_members() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    String s = "";
    s.<|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"length"),
        "expected completion list to contain String.length; got {labels:?}"
    );
}

#[test]
fn goto_definition_finds_local_method() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void foo() {}
  void bar() { <|>foo(); }
}
"#,
    );

    let loc = goto_definition(&db, file, pos).expect("expected definition location");
    let range = loc.range;

    // The `foo` declaration is on line 2 (0-based indexing, fixture has leading newline).
    assert_eq!(range.start.line, 2);
}

#[test]
fn diagnostics_include_unresolved_symbol() {
    let (db, file) = fixture_file(
        r#"
class A {
  void m() {
    baz();
  }
}
"#,
    );

    let diags = file_diagnostics(&db, file);
    assert!(
        diags.iter()
            .any(|d| d.severity == Severity::Error && d.message.contains("Cannot resolve symbol 'baz'")),
        "expected unresolved symbol diagnostic; got {diags:#?}"
    );
}
