#![cfg(feature = "ai")]

use nova_db::InMemoryFileStore;
use nova_ide::multi_token_completion_context;
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
    let offset = offset.min(text.len());
    let offset_u32 = u32::try_from(offset).unwrap_or(u32::MAX);
    let index = nova_core::LineIndex::new(text);
    let pos = index.position(text, nova_core::TextSize::from(offset_u32));
    lsp_types::Position::new(pos.line, pos.character)
}

#[test]
fn context_infers_string_receiver_and_methods() {
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

    let ctx = multi_token_completion_context(&db, file, pos);
    assert_eq!(ctx.receiver_type.as_deref(), Some("String"));
    assert!(ctx.available_methods.iter().any(|m| m == "length"));
    assert!(ctx.surrounding_code.contains("s."));
    assert!(ctx.importable_paths.is_empty());
}

#[test]
fn context_handles_stream_call_chain_receiver() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    people.stream().<|>
  }
}
"#,
    );

    let ctx = multi_token_completion_context(&db, file, pos);
    assert_eq!(ctx.receiver_type.as_deref(), Some("Stream"));
    assert!(ctx.available_methods.iter().any(|m| m == "filter"));
    assert!(ctx.available_methods.iter().any(|m| m == "map"));
    assert!(ctx.available_methods.iter().any(|m| m == "collect"));
    assert!(ctx
        .importable_paths
        .iter()
        .any(|p| p == "java.util.stream.Collectors"));
    assert!(ctx.surrounding_code.contains("people.stream()."));
}

#[test]
fn context_falls_back_to_in_file_methods_for_unknown_types() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void bar() {}

  void m() {
    Foo f = null;
    f.<|>
  }
}
"#,
    );

    let ctx = multi_token_completion_context(&db, file, pos);
    assert_eq!(ctx.receiver_type.as_deref(), Some("Foo"));
    assert!(
        ctx.available_methods.iter().any(|m| m == "bar"),
        "expected fallback to include in-file method names; got {:?}",
        ctx.available_methods
    );
}

#[test]
fn context_uses_utf16_positions_for_non_bmp_characters() {
    // The caret is after the `.` in the same line as a non-BMP character. If we
    // accidentally treat `Position.character` as a Unicode scalar offset, the
    // position would land inside the surrogate pair and we'd lose the trailing
    // `.` in the surrounding code window.
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    String s = "😀"; s.<|>
  }
}
"#,
    );

    let ctx = multi_token_completion_context(&db, file, pos);
    assert_eq!(ctx.receiver_type.as_deref(), Some("String"));
    assert!(ctx.surrounding_code.contains("😀"));
    assert!(
        ctx.surrounding_code.trim_end().ends_with('.'),
        "expected surrounding code to include the '.' before the cursor, got: {:?}",
        ctx.surrounding_code
    );
}
