use lsp_types::{CompletionItemKind, CompletionTextEdit};
use nova_db::InMemoryFileStore;
use nova_ide::completions;
use std::path::PathBuf;

const CARET: &str = "<|>";

fn offset_to_position(text: &str, offset: usize) -> lsp_types::Position {
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

    lsp_types::Position::new(line, col_utf16)
}

fn fixture(
    text_with_caret: &str,
) -> (
    InMemoryFileStore,
    nova_db::FileId,
    lsp_types::Position,
    String,
) {
    let caret_offset = text_with_caret
        .find(CARET)
        .expect("fixture must contain <|> caret marker");
    let text = text_with_caret.replace(CARET, "");
    let pos = offset_to_position(&text, caret_offset);

    let mut db = InMemoryFileStore::new();
    let path = PathBuf::from("/test.java");
    let file = db.file_id_for_path(&path);
    db.set_file_text(file, text.clone());
    (db, file, pos, text)
}

#[test]
fn completion_includes_postfix_if_for_boolean_and_replaces_full_expr() {
    let (db, file, pos, text) = fixture(
        r#"
class A {
  void m() {
    boolean cond = true;
    cond.if<|>
  }
}
"#,
    );

    let expr_start = text.find("cond.if").expect("expected cond.if in fixture");

    let items = completions(&db, file, pos);
    let item = items
        .iter()
        .find(|i| i.label == "if" && i.kind == Some(CompletionItemKind::SNIPPET))
        .expect("expected postfix `if` snippet completion");

    let edit = match item.text_edit.as_ref().expect("expected text_edit") {
        CompletionTextEdit::Edit(edit) => edit,
        other => panic!("unexpected text_edit variant: {other:?}"),
    };

    assert_eq!(edit.range.start, offset_to_position(&text, expr_start));
    assert_eq!(edit.range.end, pos);
    assert!(
        edit.new_text.contains("if (cond)"),
        "expected snippet to contain `if (cond)`; got {:?}",
        edit.new_text
    );
}

#[test]
fn completion_includes_postfix_nn_for_reference_and_replaces_full_expr() {
    let (db, file, pos, text) = fixture(
        r#"
class A {
  void m() {
    String s = "";
    s.nn<|>
  }
}
"#,
    );

    let expr_start = text.find("s.nn").expect("expected s.nn in fixture");

    let items = completions(&db, file, pos);
    let item = items
        .iter()
        .find(|i| i.label == "nn" && i.kind == Some(CompletionItemKind::SNIPPET))
        .expect("expected postfix `nn` snippet completion");

    let edit = match item.text_edit.as_ref().expect("expected text_edit") {
        CompletionTextEdit::Edit(edit) => edit,
        other => panic!("unexpected text_edit variant: {other:?}"),
    };

    assert_eq!(edit.range.start, offset_to_position(&text, expr_start));
    assert_eq!(edit.range.end, pos);
    assert!(
        edit.new_text.contains("if (s != null)"),
        "expected snippet to contain `if (s != null)`; got {:?}",
        edit.new_text
    );
}

#[test]
fn completion_includes_postfix_for_for_array_and_replaces_full_expr() {
    let (db, file, pos, text) = fixture(
        r#"
class A {
  void m() {
    String[] xs = new String[0];
    xs.for<|>
  }
}
"#,
    );

    let expr_start = text.find("xs.for").expect("expected xs.for in fixture");

    let items = completions(&db, file, pos);
    let item = items
        .iter()
        .find(|i| i.label == "for" && i.kind == Some(CompletionItemKind::SNIPPET))
        .expect("expected postfix `for` snippet completion");

    let edit = match item.text_edit.as_ref().expect("expected text_edit") {
        CompletionTextEdit::Edit(edit) => edit,
        other => panic!("unexpected text_edit variant: {other:?}"),
    };

    assert_eq!(edit.range.start, offset_to_position(&text, expr_start));
    assert_eq!(edit.range.end, pos);
    assert!(
        edit.new_text.contains("for (var"),
        "expected snippet to contain `for (var`; got {:?}",
        edit.new_text
    );
    assert!(
        edit.new_text.contains(": xs"),
        "expected snippet to reference receiver `xs`; got {:?}",
        edit.new_text
    );
}

#[test]
fn completion_includes_postfix_for_for_imported_iterable_and_replaces_full_expr() {
    let (db, file, pos, text) = fixture(
        r#"
import java.util.*;
class A {
  void m() {
    List xs = null;
    xs.for<|>
  }
}
"#,
    );

    let expr_start = text.find("xs.for").expect("expected xs.for in fixture");

    let items = completions(&db, file, pos);
    let item = items
        .iter()
        .find(|i| i.label == "for" && i.kind == Some(CompletionItemKind::SNIPPET))
        .expect("expected postfix `for` snippet completion");

    let edit = match item.text_edit.as_ref().expect("expected text_edit") {
        CompletionTextEdit::Edit(edit) => edit,
        other => panic!("unexpected text_edit variant: {other:?}"),
    };

    assert_eq!(edit.range.start, offset_to_position(&text, expr_start));
    assert_eq!(edit.range.end, pos);
    assert!(
        edit.new_text.contains("for (var"),
        "expected snippet to contain `for (var`; got {:?}",
        edit.new_text
    );
    assert!(
        edit.new_text.contains(": xs"),
        "expected snippet to reference receiver `xs`; got {:?}",
        edit.new_text
    );
}

#[test]
fn completion_includes_postfix_stream_for_list_and_replaces_full_expr() {
    let (db, file, pos, text) = fixture(
        r#"
import java.util.List;
class A {
  void m() {
    List xs = null;
    xs.stream<|>
  }
}
"#,
    );

    let expr_start = text
        .find("xs.stream")
        .expect("expected xs.stream in fixture");

    let items = completions(&db, file, pos);
    let item = items
        .iter()
        .find(|i| i.label == "stream" && i.kind == Some(CompletionItemKind::SNIPPET))
        .expect("expected postfix `stream` snippet completion");

    let edit = match item.text_edit.as_ref().expect("expected text_edit") {
        CompletionTextEdit::Edit(edit) => edit,
        other => panic!("unexpected text_edit variant: {other:?}"),
    };

    assert_eq!(edit.range.start, offset_to_position(&text, expr_start));
    assert_eq!(edit.range.end, pos);
    assert!(
        edit.new_text.contains("xs.stream()"),
        "expected snippet to contain `xs.stream()`; got {:?}",
        edit.new_text
    );
}

#[test]
fn completion_method_reference_type_receiver_includes_static_method() {
    let (db, file, pos, _text) = fixture(
        r#"
class Foo {
  static void stat() {}
}
class A {
  void m() {
    Foo::st<|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    assert!(
        items.iter().any(|i| i.label == "stat"),
        "expected completion list to contain Foo::stat; got {items:#?}"
    );
}

#[test]
fn completion_method_reference_instance_receiver_includes_method() {
    let (db, file, pos, _text) = fixture(
        r#"
class Foo {
  void inst() {}
}
class A {
  void m(Foo f) {
    f::in<|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    assert!(
        items.iter().any(|i| i.label == "inst"),
        "expected completion list to contain f::inst; got {items:#?}"
    );
}

#[test]
fn completion_method_reference_constructor_includes_new() {
    let (db, file, pos, _text) = fixture(
        r#"
class Foo {
  Foo() {}
}
class A {
  void m() {
    Foo::<|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    assert!(
        items.iter().any(|i| i.label == "new"),
        "expected completion list to contain Foo::new; got {items:#?}"
    );
}

#[test]
fn completion_includes_enum_constants_in_switch_case_labels() {
    let (db, file, pos, _text) = fixture(
        r#"
enum Color { RED, GREEN }

class A {
  void m(Color c) {
    switch (c) {
      case R<|>:
        break;
    }
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    assert!(
        items.iter().any(|item| item.label == "RED"),
        "expected enum constant completion to include `RED`; got {items:#?}"
    );
}
