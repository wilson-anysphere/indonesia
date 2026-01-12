use lsp_types::{CompletionItemKind, CompletionTextEdit};
use nova_db::InMemoryFileStore;
use nova_ide::completions;
use std::path::PathBuf;

use crate::text_fixture::{offset_to_position, CARET};

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
        edit.new_text.contains("for (String"),
        "expected snippet to contain `for (String`; got {:?}",
        edit.new_text
    );
    assert!(
        edit.new_text.contains(": xs"),
        "expected snippet to reference receiver `xs`; got {:?}",
        edit.new_text
    );
}

#[test]
fn completion_includes_postfix_for_for_fully_qualified_array_and_replaces_full_expr() {
    let (db, file, pos, text) = fixture(
        r#"
class A {
  void m() {
    java.util.List[] xs = null;
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
        edit.new_text.contains("for (java.util.List"),
        "expected snippet to contain `for (java.util.List`; got {:?}",
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
fn completion_method_reference_type_receiver_includes_instance_method_when_static_exists() {
    let (db, file, pos, _text) = fixture(
        r#"
class Foo {
  static void stat() {}
  void inst() {}
}
class A {
  void m() {
    Foo::in<|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    assert!(
        items.iter().any(|i| i.label == "inst"),
        "expected completion list to contain Foo::inst; got {items:#?}"
    );
}

#[test]
fn completion_method_reference_parameterized_type_receiver_includes_method() {
    let (db, file, pos, _text) = fixture(
        r#"
class Foo<T> {
  static void stat() {}
}
class A {
  void m() {
    Foo<String>::st<|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    assert!(
        items.iter().any(|i| i.label == "stat"),
        "expected completion list to contain Foo<String>::stat; got {items:#?}"
    );
}

#[test]
fn completion_method_reference_with_explicit_type_arguments_after_double_colon_triggers_completions(
) {
    let (db, file, pos, _text) = fixture(
        r#"
class Foo {
  static void stat() {}
}
class A {
  void m() {
    Foo::<String>st<|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    assert!(
        items.iter().any(|i| i.label == "stat"),
        "expected completion list to contain Foo::<String>stat; got {items:#?}"
    );
}

#[test]
fn completion_method_reference_array_constructor_includes_new() {
    let (db, file, pos, _text) = fixture(
        r#"
class A {
  void m() {
    int[]::<|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    assert!(
        items.iter().any(|i| i.label == "new"),
        "expected completion list to contain int[]::new; got {items:#?}"
    );
}

#[test]
fn completion_method_reference_expression_receiver_includes_method() {
    let (db, file, pos, _text) = fixture(
        r#"
class Foo {
  void inst() {}
}
class A {
  void m() {
    new Foo()::in<|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    assert!(
        items.iter().any(|i| i.label == "inst"),
        "expected completion list to contain new Foo()::inst; got {items:#?}"
    );
}

#[test]
fn completion_method_reference_expression_receiver_method_call_includes_method() {
    let (db, file, pos, _text) = fixture(
        r#"
class Foo {
  void inst() {}
}
class A {
  Foo foo() { return null; }
  void m() {
    foo()::in<|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    assert!(
        items.iter().any(|i| i.label == "inst"),
        "expected completion list to contain foo()::inst; got {items:#?}"
    );
}

#[test]
fn completion_method_reference_static_field_receiver_includes_jdk_method() {
    let (db, file, pos, _text) = fixture(
        r#"
class A {
  void m() {
    System.out::pri<|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    assert!(
        items.iter().any(|i| i.label == "println"),
        "expected completion list to contain System.out::println; got {items:#?}"
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

#[test]
fn completion_includes_enum_constants_in_switch_case_labels_for_this_field_selector() {
    let (db, file, pos, _text) = fixture(
        r#"
enum Color { RED, GREEN }

class A {
  Color color;

  void m() {
    switch (this.color) {
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

#[test]
fn completion_includes_enum_constants_in_switch_case_labels_for_qualified_field_selector() {
    let (db, file, pos, _text) = fixture(
        r#"
enum Color { RED, GREEN }

class Holder {
  Color color;
}

class A {
  void m(Holder h) {
    switch (h.color) {
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

#[test]
fn completion_suggests_boolean_literals_for_boolean_initializer() {
    let (db, file, pos, _text) = fixture(
        r#"
class A {
  void m() {
    boolean b = <|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"true"),
        "expected completion list to contain boolean literal `true`; got {labels:?}"
    );
    assert!(
        labels.contains(&"false"),
        "expected completion list to contain boolean literal `false`; got {labels:?}"
    );
}

#[test]
fn completion_suggests_string_and_null_literals_for_string_initializer() {
    let (db, file, pos, _text) = fixture(
        r#"
class A {
  void m() {
    String s = <|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"\"\""),
        "expected completion list to contain empty string literal; got {labels:?}"
    );
    assert!(
        labels.contains(&"null"),
        "expected completion list to contain `null`; got {labels:?}"
    );
}

#[test]
fn completion_suggests_zero_for_numeric_initializer() {
    let (db, file, pos, _text) = fixture(
        r#"
class A {
  void m() {
    int n = <|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"0"),
        "expected completion list to contain numeric literal `0`; got {labels:?}"
    );
}
