use nova_db::InMemoryFileStore;
use nova_ide::multi_token_completion_context;
use std::path::PathBuf;

use crate::framework_harness::{offset_to_position, CARET};

fn fixture(text_with_caret: &str) -> (InMemoryFileStore, nova_db::FileId, lsp_types::Position) {
    let caret_offset = text_with_caret
        .find(CARET)
        .expect("fixture must contain <|> caret marker");
    let text = text_with_caret.replace(CARET, "");
    let pos = offset_to_position(&text, caret_offset);

    let mut db = InMemoryFileStore::new();
    let path = PathBuf::from("/test.java");
    let file = db.file_id_for_path(&path);
    db.set_file_text(file, text);
    (db, file, pos)
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
    let receiver_ty = ctx.receiver_type.as_deref().unwrap_or("");
    assert!(
        receiver_ty.contains("String"),
        "expected receiver type to contain `String`, got {receiver_ty:?}"
    );
    assert!(ctx.available_methods.iter().any(|m| m == "length"));
    assert!(ctx.available_methods.iter().any(|m| m == "substring"));
    assert!(ctx.surrounding_code.contains("s."));
    assert!(ctx.importable_paths.is_empty());
}

#[test]
fn context_handles_stream_call_chain_receiver() {
    let (db, file, pos) = fixture(
        r#"
import java.util.List;

class Person {}

class A {
  void m() {
    List<Person> people = null;
    people.stream().<|>
  }
}
"#,
    );

    let ctx = multi_token_completion_context(&db, file, pos);
    let receiver_ty = ctx.receiver_type.as_deref().unwrap_or("");
    assert!(
        receiver_ty.contains("Stream"),
        "expected receiver type to contain `Stream`, got {receiver_ty:?}"
    );
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
fn context_handles_dotted_field_chain_receiver() {
    let (db, file, pos) = fixture(
        r#"
class B {
  String s = "x";
}

class A {
  B b = new B();

  void m() {
    this.b.s.<|>
  }
}
"#,
    );

    let ctx = multi_token_completion_context(&db, file, pos);
    let receiver_ty = ctx.receiver_type.as_deref().unwrap_or("");
    assert!(
        receiver_ty.contains("String"),
        "expected receiver type to contain `String`, got {receiver_ty:?}"
    );
    assert!(ctx.available_methods.iter().any(|m| m == "length"));
    assert!(ctx.available_methods.iter().any(|m| m == "substring"));
    assert!(ctx.surrounding_code.contains("this.b.s."));
    assert!(ctx.importable_paths.is_empty());
}

#[test]
fn context_static_receiver_lists_static_methods_only() {
    let (db, file, pos) = fixture(
        r#"
class Util {
  static int foo() { return 0; }
  int bar() { return 0; }
  static void baz() {}
}

class A {
  void m() {
    Util.<|>
  }
}
"#,
    );

    let ctx = multi_token_completion_context(&db, file, pos);
    assert!(ctx.available_methods.iter().any(|m| m == "foo"));
    assert!(ctx.available_methods.iter().any(|m| m == "baz"));
    assert!(
        !ctx.available_methods.iter().any(|m| m == "bar"),
        "expected static receiver to exclude instance methods; got {:?}",
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
    let receiver_ty = ctx.receiver_type.as_deref().unwrap_or("");
    assert!(
        receiver_ty.contains("String"),
        "expected receiver type to contain `String`, got {receiver_ty:?}"
    );
    assert!(ctx.surrounding_code.contains("😀"));
    assert!(
        ctx.surrounding_code.trim_end().ends_with('.'),
        "expected surrounding code to include the '.' before the cursor, got: {:?}",
        ctx.surrounding_code
    );
}
