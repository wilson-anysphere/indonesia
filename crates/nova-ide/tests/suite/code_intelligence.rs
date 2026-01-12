use lsp_types::{CompletionItemKind, CompletionTextEdit, InsertTextFormat};
use nova_db::InMemoryFileStore;
use nova_ide::{
    call_hierarchy_outgoing_calls, completions, document_symbols, file_diagnostics,
    find_references, goto_definition, hover, inlay_hints, prepare_call_hierarchy,
    prepare_type_hierarchy, signature_help, type_hierarchy_subtypes, type_hierarchy_supertypes,
};
use nova_types::Severity;
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

fn fixture_file(text: &str) -> (InMemoryFileStore, nova_db::FileId) {
    let mut db = InMemoryFileStore::new();
    let path = PathBuf::from("/test.java");
    let file = db.file_id_for_path(&path);
    db.set_file_text(file, text.to_string());
    (db, file)
}

fn fixture_multi(
    primary_path: PathBuf,
    primary_text_with_caret: &str,
    extra_files: Vec<(PathBuf, String)>,
) -> (InMemoryFileStore, nova_db::FileId, lsp_types::Position) {
    let caret_offset = primary_text_with_caret
        .find(CARET)
        .expect("fixture must contain <|> caret marker");
    let primary_text = primary_text_with_caret.replace(CARET, "");
    let pos = offset_to_position(&primary_text, caret_offset);

    let mut db = InMemoryFileStore::new();
    let primary_file = db.file_id_for_path(&primary_path);
    db.set_file_text(primary_file, primary_text);
    for (path, text) in extra_files {
        let id = db.file_id_for_path(&path);
        db.set_file_text(id, text);
    }

    (db, primary_file, pos)
}

fn strip_marker(text: &str, marker: &str) -> (String, usize) {
    let offset = text
        .find(marker)
        .unwrap_or_else(|| panic!("expected marker `{marker}` in fixture"));
    let cleaned = text.replacen(marker, "", 1);
    (cleaned, offset)
}
#[test]
fn completion_includes_string_members() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    String s = "🙂🙂"; s.<|>
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
fn completion_includes_array_length() {
    let (db, file, pos) = fixture(
        r#"
class A { void m(){ int[] xs=null; xs.<|> } }
"#,
    );

    let items = completions(&db, file, pos);
    assert!(
        !items.is_empty(),
        "expected non-empty completion list for array member access"
    );
    assert_eq!(
        items[0].label, "length",
        "expected array.length to rank first for empty prefix; got {:?}",
        items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>()
    );
    let item = items
        .iter()
        .find(|i| i.label == "length")
        .expect("expected completion list to contain array.length");

    assert_eq!(item.kind, Some(CompletionItemKind::FIELD));
    assert!(
        item.detail.as_deref().unwrap_or("").contains("int"),
        "expected array.length completion detail to mention int; got {:?}",
        item.detail
    );
    assert_eq!(item.insert_text.as_deref(), Some("length"));
}

#[test]
fn completion_includes_object_members_for_arrays() {
    let (db, file, pos) = fixture(
        r#"
class A { void m(){ int[] xs=null; xs.<|> } }
"#,
    );

    let items = completions(&db, file, pos);
    let item = items
        .iter()
        .find(|i| i.label == "toString")
        .expect("expected completion list to contain Object.toString for arrays");

    assert_eq!(item.kind, Some(CompletionItemKind::METHOD));
    assert_eq!(item.insert_text.as_deref(), Some("toString()"));
}

#[test]
fn completion_is_suppressed_in_char_literal() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    char c = 'a<|>';
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    assert!(
        items.is_empty(),
        "expected no completions inside char literal; got {:?}",
        items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>()
    );
}

#[test]
fn completion_is_suppressed_in_text_block() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    String s = """hel<|>lo""";
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    assert!(
        items.is_empty(),
        "expected no completions inside text block; got {:?}",
        items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>()
    );
}

#[test]
fn completion_is_suppressed_in_string_template_text() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    String s = STR."hel<|>lo";
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    assert!(
        items.is_empty(),
        "expected no completions inside string template text; got {:?}",
        items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>()
    );
}

#[test]
fn completion_is_suppressed_in_empty_string_template() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    String s = STR."<|>";
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    assert!(
        items.is_empty(),
        "expected no completions inside empty string template; got {:?}",
        items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>()
    );
}

#[test]
fn completion_in_string_template_interpolation_is_not_suppressed() {
    let (db, file, pos) = fixture(
        r#"
class A {
  int x;
  void m() {
    String s = STR."x=\{ this.<|> }";
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"x"),
        "expected completion list to contain member from interpolation expression; got {labels:?}"
    );
}

#[test]
fn completion_is_suppressed_in_string_template_interpolation_line_comment() {
    let (db, file, pos) = fixture(
        r#"
class A {
  int x;
  void m() {
    String s = STR."x=\{
      // comm<|>ent
      this.x
    }";
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    assert!(
        items.is_empty(),
        "expected no completions inside string template interpolation line comment; got {items:#?}"
    );
}

#[test]
fn completion_is_suppressed_in_string_template_interpolation_block_comment() {
    let (db, file, pos) = fixture(
        r#"
class A {
  int x;
  void m() {
    String s = STR."x=\{ /* comm<|>ent */ this.x }";
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    assert!(
        items.is_empty(),
        "expected no completions inside string template interpolation block comment; got {items:#?}"
    );
}

#[test]
fn completion_includes_this_members() {
    let (db, file, pos) = fixture(
        r#"
class A { int x; }
class B extends A {
  int y;
  void m() { this.<|> }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"y"),
        "expected completion list to contain member of current class; got {labels:?}"
    );
}

#[test]
fn completion_includes_super_members() {
    let (db, file, pos) = fixture(
        r#"
class A { int x; }
class B extends A {
  void m() { super.<|> }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"x"),
        "expected completion list to contain superclass member; got {labels:?}"
    );
}

#[test]
fn completion_in_incomplete_import_is_non_empty() {
    let (db, file, pos) = fixture(
        r#"
import java.util.<|>
class A {}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        !labels.is_empty(),
        "expected completion list to be non-empty inside incomplete import; got {labels:?}"
    );
}

#[test]
fn java_import_completion_includes_workspace_types() {
    let foo_path = PathBuf::from("/workspace/src/main/java/p/Foo.java");
    let main_path = PathBuf::from("/workspace/src/main/java/Main.java");

    let foo_text = "package p; public class Foo {}".to_string();
    let main_text = r#"
import p.<|>
class Main {}
"#;

    let (db, file, pos) = fixture_multi(main_path, main_text, vec![(foo_path, foo_text)]);

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"Foo"),
        "expected import completion list to contain Foo; got {labels:?}"
    );
}

#[test]
fn java_new_completion_includes_workspace_types() {
    let foo_path = PathBuf::from("/workspace/src/main/java/p/Foo.java");
    let main_path = PathBuf::from("/workspace/src/main/java/p/Main2.java");

    let foo_text = "package p; public class Foo {}".to_string();
    let main_text = r#"
package p;
class Main2 {
  void m() {
    new Fo<|>
  }
}
"#;

    let (db, file, pos) = fixture_multi(main_path, main_text, vec![(foo_path, foo_text)]);

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"Foo"),
        "expected `new` completion list to contain Foo; got {labels:?}"
    );
}

#[test]
fn java_new_completion_inserts_constructor_snippet_placeholders_for_workspace_type() {
    let foo_path = PathBuf::from("/workspace/src/main/java/p/Foo.java");
    let main_path = PathBuf::from("/workspace/src/main/java/p/Main2.java");

    let foo_text = "package p; public class Foo { public Foo(int x, String y) {} }".to_string();
    let main_text = r#"
package p;
class Main2 {
  void m() {
    new Fo<|>
  }
}
"#;

    let (db, file, pos) = fixture_multi(main_path, main_text, vec![(foo_path, foo_text)]);

    let items = completions(&db, file, pos);
    let item = items
        .iter()
        .find(|i| i.label == "Foo")
        .expect("expected Foo completion item");

    assert_eq!(
        item.insert_text_format,
        Some(InsertTextFormat::SNIPPET),
        "expected Foo `new` completion to use snippet insertion; got {item:#?}"
    );

    let edit = match item.text_edit.as_ref().expect("expected text_edit") {
        CompletionTextEdit::Edit(edit) => edit,
        other => panic!("unexpected text_edit variant: {other:?}"),
    };
    assert!(
        edit.new_text.contains("${1:arg0}"),
        "expected snippet to contain first constructor arg placeholder; got {:?}",
        edit.new_text
    );
    assert!(
        edit.new_text.contains("${2:arg1}"),
        "expected snippet to contain second constructor arg placeholder; got {:?}",
        edit.new_text
    );
    assert!(
        edit.new_text.ends_with(")$0"),
        "expected snippet to end with `)$0`; got {:?}",
        edit.new_text
    );
}

#[test]
fn completion_in_incomplete_import_keyword_suggests_import() {
    let (db, file, pos) = fixture("impor<|>");
    let items = completions(&db, file, pos);
    assert!(
        items.iter().any(|i| i.label == "import"),
        "expected `import` keyword completion for incomplete `impor`; got {items:#?}"
    );
}

#[test]
fn completion_in_incomplete_package_keyword_suggests_package() {
    let (db, file, pos) = fixture("packag<|>");
    let items = completions(&db, file, pos);
    assert!(
        items.iter().any(|i| i.label == "package"),
        "expected `package` keyword completion for incomplete `packag`; got {items:#?}"
    );
}

#[test]
fn completion_in_incomplete_interface_keyword_suggests_interface() {
    let (db, file, pos) = fixture("interf<|>");
    let items = completions(&db, file, pos);
    assert!(
        items.iter().any(|i| i.label == "interface"),
        "expected `interface` keyword completion for incomplete `interf`; got {items:#?}"
    );
}

#[test]
fn completion_in_incomplete_enum_keyword_suggests_enum() {
    let (db, file, pos) = fixture("enu<|>");
    let items = completions(&db, file, pos);
    assert!(
        items.iter().any(|i| i.label == "enum"),
        "expected `enum` keyword completion for incomplete `enu`; got {items:#?}"
    );
}

#[test]
fn completion_in_incomplete_record_keyword_suggests_record() {
    let (db, file, pos) = fixture("recor<|>");
    let items = completions(&db, file, pos);
    assert!(
        items.iter().any(|i| i.label == "record"),
        "expected `record` keyword completion for incomplete `recor`; got {items:#?}"
    );
}

#[test]
fn completion_in_incomplete_extends_keyword_suggests_extends() {
    let (db, file, pos) = fixture("exten<|>");
    let items = completions(&db, file, pos);
    assert!(
        items.iter().any(|i| i.label == "extends"),
        "expected `extends` keyword completion for incomplete `exten`; got {items:#?}"
    );
}

#[test]
fn completion_in_incomplete_implements_keyword_suggests_implements() {
    let (db, file, pos) = fixture("implemen<|>");
    let items = completions(&db, file, pos);
    assert!(
        items.iter().any(|i| i.label == "implements"),
        "expected `implements` keyword completion for incomplete `implemen`; got {items:#?}"
    );
}

#[test]
fn completion_in_incomplete_public_keyword_suggests_public() {
    let (db, file, pos) = fixture("publ<|>");
    let items = completions(&db, file, pos);
    assert!(
        items.iter().any(|i| i.label == "public"),
        "expected `public` keyword completion for incomplete `publ`; got {items:#?}"
    );
}

#[test]
fn completion_in_incomplete_protected_keyword_suggests_protected() {
    let (db, file, pos) = fixture("protec<|>");
    let items = completions(&db, file, pos);
    assert!(
        items.iter().any(|i| i.label == "protected"),
        "expected `protected` keyword completion for incomplete `protec`; got {items:#?}"
    );
}

#[test]
fn completion_in_incomplete_private_keyword_suggests_private() {
    let (db, file, pos) = fixture("privat<|>");
    let items = completions(&db, file, pos);
    assert!(
        items.iter().any(|i| i.label == "private"),
        "expected `private` keyword completion for incomplete `privat`; got {items:#?}"
    );
}

#[test]
fn completion_in_incomplete_static_keyword_suggests_static() {
    let (db, file, pos) = fixture("stat<|>");
    let items = completions(&db, file, pos);
    assert!(
        items.iter().any(|i| i.label == "static"),
        "expected `static` keyword completion for incomplete `stat`; got {items:#?}"
    );
}

#[test]
fn completion_in_import_static_keyword_position_suggests_static() {
    let (db, file, pos) = fixture(
        r#"
import stat<|>
class A {}
"#,
    );
    let items = completions(&db, file, pos);

    let static_item = items
        .iter()
        .find(|i| i.label == "static")
        .unwrap_or_else(|| panic!("expected `static` completion item; got {items:#?}"));

    let Some(CompletionTextEdit::Edit(edit)) = static_item.text_edit.as_ref() else {
        panic!("expected `static` completion to contain text_edit; got {static_item:#?}");
    };

    assert_eq!(
        edit.new_text, "static ",
        "expected `static` completion to insert the keyword plus trailing space; got {static_item:#?}"
    );
}

#[test]
fn completion_in_incomplete_final_keyword_suggests_final() {
    let (db, file, pos) = fixture("fina<|>");
    let items = completions(&db, file, pos);
    assert!(
        items.iter().any(|i| i.label == "final"),
        "expected `final` keyword completion for incomplete `fina`; got {items:#?}"
    );
}

#[test]
fn completion_in_incomplete_abstract_keyword_suggests_abstract() {
    let (db, file, pos) = fixture("abstrac<|>");
    let items = completions(&db, file, pos);
    assert!(
        items.iter().any(|i| i.label == "abstract"),
        "expected `abstract` keyword completion for incomplete `abstrac`; got {items:#?}"
    );
}

#[test]
fn completion_includes_member_from_parameter_receiver() {
    let (db, file, pos) = fixture(
        r#"
class Foo { int x; }
class A { void m(Foo foo){ foo.<|> } }
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"x"),
        "expected completion list to contain Foo.x when completing on param receiver; got {labels:?}"
    );
}

#[test]
fn completion_in_incomplete_call_does_not_panic() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    "x".substring(<|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    assert!(
        !items.is_empty(),
        "expected completion list to be non-empty inside incomplete call; got {items:#?}"
    );
}

#[test]
fn completion_infers_expected_return_type_for_return_statement() {
    let (db, file, pos) = fixture(
        r#"
class A {
  String m(){
    int i = 0;
    return <|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"\"\""),
        "expected completion list to include string literal snippet; got {items:#?}"
    );
    assert!(
        !labels.contains(&"i"),
        "expected incompatible int local to be filtered out in String return; got {labels:?}"
    );
}

#[test]
fn completion_includes_locals_in_nested_class_method() {
    let (db, file, pos) = fixture(
        r#"
class Outer {
  class Inner {
    void m() {
      int x = 0;
      <|>
    }
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    assert!(
        items.iter().any(|i| i.label == "x"),
        "expected completion list to include local `x` inside nested class method; got {items:#?}"
    );
}

#[test]
fn completion_at_eof_after_whitespace_is_deterministic() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {}
}
   <|>"#,
    );

    let items1 = completions(&db, file, pos);
    let items2 = completions(&db, file, pos);

    let labels1: Vec<_> = items1.iter().map(|i| i.label.clone()).collect();
    let labels2: Vec<_> = items2.iter().map(|i| i.label.clone()).collect();

    assert!(
        !labels1.is_empty(),
        "expected completion list to be non-empty at EOF; got {labels1:?}"
    );
    assert_eq!(
        labels1, labels2,
        "expected completion list to be deterministic"
    );
}

#[test]
fn completion_includes_enum_constants_as_static_members() {
    let enum_path = PathBuf::from("/workspace/src/main/java/p/Color.java");
    let main_path = PathBuf::from("/workspace/src/main/java/p/Main.java");

    let enum_text = r#"
package p;
enum Color { RED, GREEN }
"#
    .to_string();

    let main_text = r#"
package p;
class Main {
  void m() {
    Color.<|>
  }
}
"#;

    let (db, file, pos) = fixture_multi(main_path, main_text, vec![(enum_path, enum_text)]);

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"RED"),
        "expected completion list to contain enum constant Color.RED; got {labels:?}"
    );
}

#[test]
fn completion_parameter_receiver_works_in_package() {
    let foo_path = PathBuf::from("/workspace/src/main/java/p/Foo.java");
    let main_path = PathBuf::from("/workspace/src/main/java/p/Main.java");

    let foo_text = "package p; class Foo { void bar() {} }".to_string();
    let main_text = r#"package p; class Main { void m(Foo f) { f.<|> } }"#;

    let (db, file, pos) = fixture_multi(main_path, main_text, vec![(foo_path, foo_text)]);

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"bar"),
        "expected completion list to contain Foo.bar for parameter receiver; got {labels:?}"
    );
}

#[test]
fn completion_local_method_inserts_snippet_placeholders_for_params() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void foo(int x, String y) {}
  void m(){ fo<|> }
}
"#,
    );

    let items = completions(&db, file, pos);
    let item = items
        .iter()
        .find(|i| i.label == "foo")
        .expect("expected foo completion item");

    assert_eq!(
        item.insert_text_format,
        Some(InsertTextFormat::SNIPPET),
        "expected foo completion to use snippet format; got {item:#?}"
    );

    let insert = item
        .insert_text
        .as_deref()
        .expect("expected foo completion to have insert_text");
    assert!(
        insert.contains("${1:x}"),
        "expected snippet to contain first param placeholder; got {insert:?}"
    );
    assert!(
        insert.contains("${2:y}"),
        "expected snippet to contain second param placeholder; got {insert:?}"
    );
}

#[test]
fn completion_member_method_uses_snippet_placeholders_for_arity() {
    let (db, file, pos) = fixture(
        r#"
import java.util.*;
class A{
  void m(){
    List l=null;
    l.g<|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let item = items
        .iter()
        .find(|i| i.label == "get")
        .expect("expected get completion item");

    let insert = item
        .insert_text
        .as_deref()
        .expect("expected get completion to have insert_text");
    assert!(
        insert.contains("${1:arg0}"),
        "expected snippet to contain arg0 placeholder; got {insert:?}"
    );
    assert!(
        insert.ends_with(")$0"),
        "expected snippet to end with `)$0`; got {insert:?}"
    );
}

#[test]
fn completion_qualified_type_name_is_not_confused_by_other_method_params() {
    let (db, file, pos) = fixture(
        r#"
import java.util.Map;

class Main {
  void other(Map Map) {}

  void m() {
    Map.En<|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"Entry"),
        "expected completion list to contain nested type Map.Entry; got {labels:?}"
    );
    assert!(
        !labels.contains(&"size"),
        "expected type receiver completion not to include Map.size instance method; got {labels:?}"
    );
}

#[test]
fn completion_qualified_type_name_is_not_confused_by_out_of_scope_locals() {
    let (db, file, pos) = fixture(
        r#"
import java.util.Map;

class Main {
  void m() {
    if (true) {
      Map Map = null;
    }
    Map.En<|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"Entry"),
        "expected completion list to contain nested type Map.Entry; got {labels:?}"
    );
    assert!(
        !labels.contains(&"size"),
        "expected type receiver completion not to include Map.size instance method; got {labels:?}"
    );
}

#[test]
fn completion_deduplicates_items_by_label_and_kind() {
    // Member completions come from two sources:
    // - semantic member enumeration via `TypeStore` (source types/JDK)
    // - workspace member enumeration via the Lombok completion provider
    //
    // When both provide the same label/kind, the final list should contain only
    // one item.
    let (db, file, pos) = fixture(
        r#"
class Stream {
  Stream filter() { return this; }
  Stream map() { return this; }
  Stream collect() { return this; }
}

class A {
  void m() {
    Stream s = new Stream();
    s.<|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);

    let filter_methods: Vec<_> = items
        .iter()
        .filter(|i| i.label == "filter" && i.kind == Some(lsp_types::CompletionItemKind::METHOD))
        .collect();

    assert_eq!(
        filter_methods.len(),
        1,
        "expected exactly one `filter` method completion; got {filter_methods:#?}"
    );

    // The dedup policy should keep the richer completion item (which includes
    // signature detail).
    assert!(
        filter_methods[0].detail.is_some(),
        "expected `filter` completion to keep the item with `detail`; got {filter_methods:#?}"
    );
}

#[test]
fn completion_includes_chained_call_receiver_members_for_jdk_type() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    "x".substring(0).<|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"length"),
        "expected completion list to contain String.length after substring(); got {labels:?}"
    );
}

#[test]
fn completion_includes_chained_call_receiver_members_for_local_types() {
    let (db, file, pos) = fixture(
        r#"
class Foo { Bar bar(){return null;} }
class Bar { int x; }
class A {
  void m() {
    new Foo().bar().<|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"x"),
        "expected completion list to contain Bar.x after bar(); got {labels:?}"
    );
}

#[test]
fn completion_includes_chained_call_receiver_members_in_field_initializer() {
    let (db, file, pos) = fixture(
        r#"
class Foo { Bar bar(){return null;} }
class Bar { int x; }
class A {
  Bar b = new Foo().bar().<|>;
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"x"),
        "expected completion list to contain Bar.x after bar() in field initializer; got {labels:?}"
    );
}

#[test]
fn completion_includes_inherited_members() {
    let (db, file, pos) = fixture(
        r#"
class A { void a(){} }
class B extends A {
  void b(){}
  void m(){ new B().<|> }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"a"),
        "expected completion list to contain inherited method `a`; got {labels:?}"
    );
    assert!(
        labels.contains(&"b"),
        "expected completion list to contain method `b`; got {labels:?}"
    );
}

#[test]
fn completion_includes_detail_for_local_var_type() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    int count = 0;
    co<|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let item = items
        .iter()
        .find(|i| i.label == "count")
        .expect("expected local var completion item");

    assert_eq!(item.detail.as_deref(), Some("int"));
}

#[test]
fn completion_includes_interface_members() {
    let (db, file, pos) = fixture(
        r#"
interface I { void i(); }
class A implements I {
  void a(){}
  void m(){ new A().<|> }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"i"),
        "expected completion list to contain interface method `i`; got {labels:?}"
    );
    assert!(
        labels.contains(&"a"),
        "expected completion list to contain method `a`; got {labels:?}"
    );
}

#[test]
fn completion_includes_string_member_detail_with_return_type() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    "x".<|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let item = items
        .iter()
        .find(|i| i.label == "substring")
        .expect("expected substring completion item");

    let detail = item.detail.as_deref().unwrap_or("");
    assert!(
        detail.contains("substring(") && detail.contains("String"),
        "expected detail to contain a signature with return type; got {detail:?}"
    );
}

#[test]
fn completion_includes_string_members_for_string_literal_receiver() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    "x-y".<|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"length"),
        "expected completion list to contain String.length for string literal receiver; got {labels:?}"
    );
}

#[test]
fn completion_dedups_overridden_members() {
    let (db, file, pos) = fixture(
        r#"
class A { void a(){} }
class B extends A {
  void a(){} // override
  void m(){ new B().<|> }
}
"#,
    );

    let items = completions(&db, file, pos);
    let a_items: Vec<_> = items.iter().filter(|i| i.label == "a").collect();
    assert_eq!(
        a_items.len(),
        1,
        "expected a single completion item for overridden method `a`; got {a_items:#?}"
    );

    let origin = a_items[0]
        .data
        .as_ref()
        .and_then(|data| data.get("nova"))
        .and_then(|nova| nova.get("member_origin"))
        .and_then(|origin| origin.as_str());
    assert_eq!(
        origin,
        Some("direct"),
        "expected overridden method completion to come from receiver type; got {a_items:#?}"
    );
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
    let insert_text = if_item.insert_text.as_deref().unwrap_or("");
    assert!(
        insert_text.contains("if ("),
        "expected snippet to contain `if (`; got {insert_text:?}"
    );
}

#[test]
fn completion_includes_null_literal() {
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
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"null"),
        "expected completion list to contain `null`; got {labels:?}"
    );
}

#[test]
fn completion_includes_true_literal_with_prefix() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    tr<|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"true"),
        "expected completion list to contain `true`; got {labels:?}"
    );
}

#[test]
fn completion_includes_lambda_snippet_for_functional_interface_expected_type() {
    let (db, file, pos) = fixture(
        r#"
interface Fun { int apply(int x); }
class A {
  void m() {
    Fun f = <|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let lambda_item = items
        .iter()
        .find(|item| {
            item.kind == Some(lsp_types::CompletionItemKind::SNIPPET)
                && item
                    .insert_text
                    .as_deref()
                    .is_some_and(|text| text.contains("->"))
        })
        .expect("expected completion list to contain a lambda snippet item");

    assert_eq!(
        lambda_item.insert_text_format,
        Some(InsertTextFormat::SNIPPET),
        "expected lambda completion to use snippet insert text format; got {lambda_item:#?}"
    );
}

#[test]
fn completion_includes_lambda_snippet_for_jdk_function_expected_type() {
    let (db, file, pos) = fixture(
        r#"
import java.util.function.Function;
class A {
  void m() {
    Function f = <|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let lambda_item = items
        .iter()
        .find(|item| {
            item.kind == Some(lsp_types::CompletionItemKind::SNIPPET)
                && item
                    .insert_text
                    .as_deref()
                    .is_some_and(|text| text.contains("->"))
        })
        .expect("expected completion list to contain a lambda snippet item for java.util.function.Function");

    assert_eq!(
        lambda_item.insert_text_format,
        Some(InsertTextFormat::SNIPPET),
        "expected lambda completion to use snippet insert text format; got {lambda_item:#?}"
    );
}

#[test]
fn completion_includes_lambda_snippet_in_receiver_call_argument_expected_type() {
    let (db, file, pos) = fixture(
        r#"
interface Fun { int apply(int x); }
class B { void accept(Fun f) {} }
class A {
  void m() {
    B b = new B();
    b.accept(<|>);
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let lambda_item = items
        .iter()
        .find(|item| {
            item.kind == Some(lsp_types::CompletionItemKind::SNIPPET)
                && item
                    .insert_text
                    .as_deref()
                    .is_some_and(|text| text.contains("->"))
        })
        .expect(
            "expected completion list to contain a lambda snippet item in receiver call argument",
        );

    assert_eq!(
        lambda_item.insert_text_format,
        Some(InsertTextFormat::SNIPPET),
        "expected lambda completion to use snippet insert text format; got {lambda_item:#?}"
    );
}

#[test]
fn completion_does_not_include_lambda_snippet_for_nonfunctional_expected_type() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    String s = <|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    assert!(
        !items.iter().any(|item| {
            item.kind == Some(lsp_types::CompletionItemKind::SNIPPET)
                && item
                    .insert_text
                    .as_deref()
                    .is_some_and(|text| text.contains("->"))
        }),
        "expected completion list to not contain a lambda snippet item; got {items:#?}"
    );
}

#[test]
fn completion_in_call_argument_filters_incompatible_values_for_int() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void takeInt(int x) {}
  void m() {
    String s = "";
    int n = 0;
    takeInt(<|>);
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"n"),
        "expected completion list to contain int variable `n`; got {labels:?}"
    );
    assert!(
        !labels.contains(&"s"),
        "expected completion list to exclude String variable `s` for int parameter; got {labels:?}"
    );
}

#[test]
fn completion_in_call_argument_prefers_matching_string_values() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void takeString(String x) {}
  void m() {
    String s = "";
    int n = 0;
    takeString(<|>);
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"s"),
        "expected completion list to contain String variable `s`; got {labels:?}"
    );
    assert!(
        !labels.contains(&"n"),
        "expected completion list to exclude int variable `n` for String parameter; got {labels:?}"
    );
}

#[test]
fn completion_in_call_argument_uses_active_parameter_after_comma() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void takeIntString(int x, String y) {}
  void m() {
    String s = "";
    int n = 0;
    takeIntString(n, <|>);
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"s"),
        "expected completion list to contain String variable `s` for second parameter; got {labels:?}"
    );
    assert!(
        !labels.contains(&"n"),
        "expected completion list to exclude int variable `n` for second parameter; got {labels:?}"
    );
}

#[test]
fn completion_in_call_argument_ignores_commas_inside_array_initializer() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void takeIntsString(int[] xs, String y) {}
  void m() {
    String s = "";
    int n = 0;
    takeIntsString(new int[]{1, 2}, <|>);
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"s"),
        "expected completion list to contain String variable `s` for second parameter; got {labels:?}"
    );
    assert!(
        !labels.contains(&"n"),
        "expected completion list to exclude int variable `n` for second parameter; got {labels:?}"
    );
}

#[test]
fn completion_in_call_argument_ignores_commas_inside_generic_type_args() {
    let (db, file, pos) = fixture(
        r#"
import java.util.*;
class A {
  void takeMapString(HashMap<String, Integer> xs, String y) {}
  void m() {
    String s = "";
    int n = 0;
    takeMapString(new HashMap<String, Integer>(), <|>);
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"s"),
        "expected completion list to contain String variable `s` for second parameter; got {labels:?}"
    );
    assert!(
        !labels.contains(&"n"),
        "expected completion list to exclude int variable `n` for second parameter; got {labels:?}"
    );
}

#[test]
fn completion_includes_jdk_type_names_in_expression_context() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void take(Object x) {}
  void m() {
    take(Ma<|>);
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"Math"),
        "expected completion list to contain Math; got {labels:?}"
    );

    let math = items
        .iter()
        .find(|item| item.label == "Math")
        .expect("expected Math completion item");
    assert_eq!(math.kind, Some(CompletionItemKind::CLASS));
    assert_eq!(math.detail.as_deref(), Some("java.lang.Math"));
    assert!(
        math.additional_text_edits
            .as_ref()
            .is_none_or(|edits| edits.is_empty()),
        "expected Math completion to not require an import; got additional_text_edits={:?}",
        math.additional_text_edits
    );
}

#[test]
fn completion_includes_explicitly_imported_type_names_in_expression_context() {
    let (db, file, pos) = fixture(
        r#"
import java.util.List;
class A {
  void take(Object x) {}
  void m() {
    take(Li<|>);
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"List"),
        "expected completion list to contain List; got {labels:?}"
    );

    let list = items
        .iter()
        .find(|item| item.label == "List")
        .expect("expected List completion item");
    assert_eq!(list.kind, Some(CompletionItemKind::INTERFACE));
    assert_eq!(list.detail.as_deref(), Some("java.util.List"));
    assert!(
        list.additional_text_edits
            .as_ref()
            .is_none_or(|edits| edits.is_empty()),
        "expected explicitly imported List completion to not add a duplicate import; got additional_text_edits={:?}",
        list.additional_text_edits
    );
}

#[test]
fn completion_filters_incompatible_items_in_string_initializer() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    String s = "";
    int n = 0;
    String x = <|>;
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"s"),
        "expected completion list to contain `s`; got {labels:?}"
    );
    assert!(
        !labels.contains(&"n"),
        "expected completion list to not contain incompatible `n`; got {labels:?}"
    );
}

#[test]
fn completion_keeps_compatible_items_in_int_initializer() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    int n = 0;
    int x = <|>;
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"n"),
        "expected completion list to contain `n`; got {labels:?}"
    );
}

#[test]
fn completion_suggests_arraylist_for_list_expected_type() {
    let (db, file, pos) = fixture(
        r#"
import java.util.*;
class A {
  void m() {
    List l = <|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    assert!(
        items.iter().any(|i| i.label.contains("new ArrayList")),
        "expected completion list to contain `new ArrayList`; got {items:#?}"
    );
}

#[test]
fn completion_suggests_arraylist_adds_import_when_not_in_scope() {
    let (db, file, pos) = fixture(
        r#"
import java.util.List;
class A {
  void m() {
    List l = <|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let item = items
        .iter()
        .find(|i| i.label.contains("new ArrayList"))
        .expect("expected completion list to contain `new ArrayList`");
    let edits = item
        .additional_text_edits
        .as_ref()
        .expect("expected `new ArrayList` completion to add an import");
    assert!(
        edits.iter().any(|e| e.new_text.contains("import java.util.ArrayList;")),
        "expected import edit for java.util.ArrayList; got {edits:#?}"
    );
}

#[test]
fn completion_suggests_local_impl_for_interface_expected_type() {
    let (db, file, pos) = fixture(
        r#"
interface I {}
class Impl implements I {}
class A {
  void m() {
    I x = <|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    assert!(
        items.iter().any(|i| i.label.contains("new Impl")),
        "expected completion list to contain `new Impl`; got {items:#?}"
    );
}

#[test]
fn completion_in_boolean_condition_filters_to_boolean_typed_locals() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    boolean b = true;
    int n = 0;
    if (<|>) {}
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"b"),
        "expected completion list to contain boolean local `b`; got {labels:?}"
    );
    assert!(
        !labels.contains(&"n"),
        "expected completion list to not contain non-boolean local `n`; got {labels:?}"
    );
}

#[test]
fn completion_in_while_condition_includes_true_literal() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    while (tr<|>) {}
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"true"),
        "expected completion list to contain `true`; got {labels:?}"
    );
}

#[test]
fn completion_in_for_condition_filters_to_boolean_typed_locals() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    boolean b = true;
    int n = 0;
    for (; <|>; ) {}
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"b"),
        "expected completion list to contain boolean local `b`; got {labels:?}"
    );
    assert!(
        !labels.contains(&"n"),
        "expected completion list to not contain non-boolean local `n`; got {labels:?}"
    );
}

#[test]
fn completion_in_do_while_condition_filters_to_boolean_typed_locals() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    boolean b = true;
    int n = 0;
    do {} while (<|>);
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"b"),
        "expected completion list to contain boolean local `b`; got {labels:?}"
    );
    assert!(
        !labels.contains(&"n"),
        "expected completion list to not contain non-boolean local `n`; got {labels:?}"
    );
}

#[test]
fn completion_in_ternary_condition_filters_to_boolean_typed_locals() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    boolean b = true;
    int n = 0;
    int x = <|> ? 1 : 2;
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"b"),
        "expected completion list to contain boolean local `b`; got {labels:?}"
    );
    assert!(
        !labels.contains(&"n"),
        "expected completion list to not contain non-boolean local `n`; got {labels:?}"
    );
}

#[test]
fn completion_includes_javadoc_param_snippet() {
    let (db, file, pos) = fixture(
        r#"
/**
 * @par<|>
 */
void m(int x) {}
"#,
    );

    let items = completions(&db, file, pos);
    let item = items
        .iter()
        .find(|i| i.label == "@param")
        .expect("expected @param snippet completion");

    assert_eq!(item.kind, Some(lsp_types::CompletionItemKind::SNIPPET));
    assert_eq!(item.insert_text_format, Some(InsertTextFormat::SNIPPET));

    let edit = match item.text_edit.as_ref().expect("expected text_edit") {
        CompletionTextEdit::Edit(edit) => edit,
        other => panic!("unexpected text_edit variant: {other:?}"),
    };
    assert_eq!(edit.new_text, "@param ${1:name} $0");
}

#[test]
fn completion_includes_javadoc_param_snippet_with_method_param_name() {
    let (db, file, pos) = fixture(
        r#"
class A {
  /**
   * @par<|>
   */
  void m(int x) {}
}
"#,
    );

    let items = completions(&db, file, pos);
    let item = items
        .iter()
        .find(|i| i.label == "@param x")
        .expect("expected @param completion to include method parameter name `x`");

    assert_eq!(item.kind, Some(lsp_types::CompletionItemKind::SNIPPET));
    assert_eq!(item.insert_text_format, Some(InsertTextFormat::SNIPPET));

    let edit = match item.text_edit.as_ref().expect("expected text_edit") {
        CompletionTextEdit::Edit(edit) => edit,
        other => panic!("unexpected text_edit variant: {other:?}"),
    };
    assert_eq!(edit.new_text, "@param ${1:x} $0");
}

#[test]
fn completion_includes_javadoc_return_snippet() {
    let (db, file, pos) = fixture(
        r#"
/**
 * @ret<|>
 */
int m() { return 0; }
"#,
    );

    let items = completions(&db, file, pos);
    let item = items
        .iter()
        .find(|i| i.label == "@return")
        .expect("expected @return snippet completion");

    assert_eq!(item.kind, Some(lsp_types::CompletionItemKind::SNIPPET));
    assert_eq!(item.insert_text_format, Some(InsertTextFormat::SNIPPET));

    let edit = match item.text_edit.as_ref().expect("expected text_edit") {
        CompletionTextEdit::Edit(edit) => edit,
        other => panic!("unexpected text_edit variant: {other:?}"),
    };
    assert_eq!(edit.new_text, "@return $0");
}

#[test]
fn completion_includes_javadoc_throws_snippet() {
    let (db, file, pos) = fixture(
        r#"
/**
 * @thr<|>
 */
void m() {}
"#,
    );

    let items = completions(&db, file, pos);
    let item = items
        .iter()
        .find(|i| i.label == "@throws")
        .expect("expected @throws snippet completion");

    assert_eq!(item.kind, Some(lsp_types::CompletionItemKind::SNIPPET));
    assert_eq!(item.insert_text_format, Some(InsertTextFormat::SNIPPET));

    let edit = match item.text_edit.as_ref().expect("expected text_edit") {
        CompletionTextEdit::Edit(edit) => edit,
        other => panic!("unexpected text_edit variant: {other:?}"),
    };
    assert_eq!(edit.new_text, "@throws ${1:Exception} $0");
}

#[test]
fn completion_includes_javadoc_see_snippet() {
    let (db, file, pos) = fixture(
        r#"
/**
 * @se<|>
 */
void m() {}
"#,
    );

    let items = completions(&db, file, pos);
    let item = items
        .iter()
        .find(|i| i.label == "@see")
        .expect("expected @see snippet completion");

    assert_eq!(item.kind, Some(lsp_types::CompletionItemKind::SNIPPET));
    assert_eq!(item.insert_text_format, Some(InsertTextFormat::SNIPPET));

    let edit = match item.text_edit.as_ref().expect("expected text_edit") {
        CompletionTextEdit::Edit(edit) => edit,
        other => panic!("unexpected text_edit variant: {other:?}"),
    };
    assert_eq!(edit.new_text, "@see ${1:Reference} $0");
}

#[test]
fn completion_includes_javadoc_inline_link_snippet() {
    let (db, file, pos) = fixture(
        r#"
/**
 * {@li<|>
 */
void m() {}
"#,
    );

    let items = completions(&db, file, pos);
    let item = items
        .iter()
        .find(|i| i.label == "{@link}")
        .expect("expected {@link} snippet completion");

    assert_eq!(item.kind, Some(lsp_types::CompletionItemKind::SNIPPET));
    assert_eq!(item.insert_text_format, Some(InsertTextFormat::SNIPPET));

    let edit = match item.text_edit.as_ref().expect("expected text_edit") {
        CompletionTextEdit::Edit(edit) => edit,
        other => panic!("unexpected text_edit variant: {other:?}"),
    };
    assert_eq!(edit.new_text, "{@link ${1:TypeOrMember}}$0");
}

#[test]
fn completion_includes_javadoc_inline_link_snippet_after_open_brace() {
    let (db, file, pos) = fixture(
        r#"
/**
 * {<|>
 */
void m() {}
"#,
    );

    let items = completions(&db, file, pos);
    let item = items
        .iter()
        .find(|i| i.label == "{@link}")
        .expect("expected {@link} snippet completion after `{`");

    assert_eq!(item.kind, Some(lsp_types::CompletionItemKind::SNIPPET));
    assert_eq!(item.insert_text_format, Some(InsertTextFormat::SNIPPET));

    let edit = match item.text_edit.as_ref().expect("expected text_edit") {
        CompletionTextEdit::Edit(edit) => edit,
        other => panic!("unexpected text_edit variant: {other:?}"),
    };
    assert_eq!(edit.new_text, "{@link ${1:TypeOrMember}}$0");
}

#[test]
fn annotation_attribute_completion_suggests_elements() {
    let anno_path = PathBuf::from("/workspace/src/main/java/p/MyAnno.java");
    let java_path = PathBuf::from("/workspace/src/main/java/p/Main.java");

    let anno_text = r#"package p; public @interface MyAnno { String value(); int count(); }"#;
    let java_text = r#"package p; @MyAnno(co<|>) class Main {}"#;

    let (db, file, pos) = fixture_multi(
        java_path,
        java_text,
        vec![(anno_path, anno_text.to_string())],
    );

    let items = completions(&db, file, pos);
    let count = items
        .iter()
        .find(|i| i.label == "count")
        .expect("expected completion list to include MyAnno.count");
    let insert = count.insert_text.as_deref().unwrap_or("");
    assert!(
        insert.contains("count ="),
        "expected completion insert text to contain `count =`; got {insert:?}"
    );
}

#[test]
fn annotation_attribute_completion_filters_already_present_elements() {
    let anno_path = PathBuf::from("/workspace/src/main/java/p/MyAnno.java");
    let java_path = PathBuf::from("/workspace/src/main/java/p/Main.java");

    let anno_text = r#"package p; public @interface MyAnno { String value(); int count(); }"#;
    let java_text = r#"package p; @MyAnno(count = 1, <|>) class Main {}"#;

    let (db, file, pos) = fixture_multi(
        java_path,
        java_text,
        vec![(anno_path, anno_text.to_string())],
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"value"),
        "expected completion list to include MyAnno.value; got {labels:?}"
    );
    assert!(
        !labels.contains(&"count"),
        "expected completion list to not include MyAnno.count twice; got {labels:?}"
    );
}

#[test]
fn completion_instance_members_exclude_static() {
    let (db, file, pos) = fixture(
        r#"
class Foo {
  int inst;
  int m() { return 0; }
  static int sm() { return 0; }
}

class A {
  void test() {
    Foo f = new Foo();
    f.<|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"m"),
        "expected completion list to contain Foo.m; got {labels:?}"
    );
    assert!(
        labels.contains(&"inst"),
        "expected completion list to contain Foo.inst; got {labels:?}"
    );
    assert!(
        !labels.contains(&"sm"),
        "expected completion list to NOT contain Foo.sm for instance access; got {labels:?}"
    );
}

#[test]
fn completion_static_members_exclude_instance() {
    let (db, file, pos) = fixture(
        r#"
class Foo {
  int inst;
  int m() { return 0; }
  static int sm() { return 0; }
}

class A {
  void test() {
    Foo.<|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"sm"),
        "expected completion list to contain Foo.sm for static access; got {labels:?}"
    );
    assert!(
        !labels.contains(&"m"),
        "expected completion list to NOT contain Foo.m for static access; got {labels:?}"
    );
    assert!(
        !labels.contains(&"inst"),
        "expected completion list to NOT contain Foo.inst for static access; got {labels:?}"
    );
}

#[test]
fn completion_this_receiver_works() {
    let (db, file, pos) = fixture(
        r#"
class Foo {
  int inst;
  void m() {}
  void test() { this.<|> }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"inst"),
        "expected completion list to contain this.inst; got {labels:?}"
    );
    assert!(
        labels.contains(&"m"),
        "expected completion list to contain this.m; got {labels:?}"
    );
}

#[test]
fn completion_super_receiver_works() {
    let (db, file, pos) = fixture(
        r#"
class Base { void base() {} }
class Foo extends Base { void test() { super.<|> } }
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"base"),
        "expected completion list to contain super.base; got {labels:?}"
    );
}

#[test]
fn completion_parameter_receiver_works() {
    let (db, file, pos) = fixture(
        r#"
class Foo {
  int inst;
  int m() { return 0; }
  static int sm() { return 0; }
}

class A {
  void test(Foo f) {
    f.<|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"m"),
        "expected completion list to contain Foo.m for parameter receiver; got {labels:?}"
    );
}

#[test]
fn completion_new_expression_includes_arraylist_with_star_import() {
    let (db, file, pos) = fixture(
        r#"
 import java.util.*;
 class A {
   void m() {
     Object x = new Arr<|>
   }
 }
 "#,
    );

    let items = completions(&db, file, pos);
    let item = items
        .iter()
        .find(|i| i.label == "ArrayList")
        .expect("expected ArrayList completion item");

    assert_eq!(
        item.additional_text_edits, None,
        "expected no additional_text_edits when ArrayList is covered by `import java.util.*;`"
    );

    assert_eq!(
        item.insert_text_format,
        Some(InsertTextFormat::SNIPPET),
        "expected ArrayList completion to use snippet insertion"
    );

    let edit = match item.text_edit.as_ref().expect("expected text_edit") {
        CompletionTextEdit::Edit(edit) => edit,
        other => panic!("unexpected text_edit variant: {other:?}"),
    };
    assert!(
        edit.new_text.starts_with("ArrayList("),
        "expected snippet insertion to start with `ArrayList(`; got {:?}",
        edit.new_text
    );
}

#[test]
fn completion_in_package_declaration_uses_workspace_packages_and_replaces_segment_only() {
    let file_a_path = PathBuf::from("/workspace/src/main/java/com/foo/A.java");
    let file_b_path = PathBuf::from("/workspace/src/main/java/com/B.java");

    let file_a_text = "package com.foo; class A{}".to_string();
    let file_b_text = "package com.f<|>; class B{}";

    let (db, file, pos) = fixture_multi(file_b_path, file_b_text, vec![(file_a_path, file_a_text)]);

    let without_caret = file_b_text.replace("<|>", "");
    let f_start = without_caret
        .find("com.f")
        .expect("expected `com.f` in fixture")
        + "com.".len();

    let items = completions(&db, file, pos);
    assert!(
        items.iter().any(|i| i.label == "foo" || i.label == "foo."),
        "expected workspace package segment completion; got {items:#?}"
    );

    let item = items
        .iter()
        .find(|i| i.label == "foo" || i.label == "foo.")
        .expect("expected foo completion item");
    let edit = match item.text_edit.as_ref().expect("expected text_edit") {
        CompletionTextEdit::Edit(edit) => edit,
        other => panic!("unexpected text_edit variant: {other:?}"),
    };
    assert_eq!(
        edit.range.start,
        offset_to_position(&without_caret, f_start),
        "expected completion to replace only the current segment"
    );
    assert_eq!(edit.range.end, pos);
}

#[test]
fn completion_in_module_info_requires_suggests_java_base() {
    let module_path = PathBuf::from("/workspace/module-info.java");
    let (db, file, pos) = fixture_multi(module_path, "module my.mod { requires ja<|> }", vec![]);

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"java.base"),
        "expected module-info completion to contain java.base; got {labels:?}"
    );
}

#[test]
fn completion_in_module_info_requires_does_not_suggest_modifiers_while_completing_module_name() {
    let module_path = PathBuf::from("/workspace/module-info.java");
    let (db, file, pos) = fixture_multi(module_path, "module my.mod { requires ja<|> }", vec![]);

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        !labels.contains(&"static"),
        "did not expect requires completion to suggest `static` while completing module name; got {labels:?}"
    );
    assert!(
        !labels.contains(&"transitive"),
        "did not expect requires completion to suggest `transitive` while completing module name; got {labels:?}"
    );
}

#[test]
fn completion_in_empty_module_info_suggests_module_declaration_snippet() {
    let module_path = PathBuf::from("/workspace/module-info.java");
    let (db, file, pos) = fixture_multi(module_path, "<|>", vec![]);

    let items = completions(&db, file, pos);
    let item = items
        .iter()
        .find(|i| i.label == "module")
        .expect("expected module snippet completion");

    assert_eq!(
        item.insert_text_format,
        Some(InsertTextFormat::SNIPPET),
        "expected module completion to be a snippet; got {item:#?}"
    );
    assert!(
        item.insert_text
            .as_deref()
            .is_some_and(|t| t.contains("module ${1:name}")),
        "expected module snippet to contain placeholder text; got {item:#?}"
    );
}

#[test]
fn completion_in_module_info_exports_suggests_workspace_package_segment() {
    let module_path = PathBuf::from("/workspace/module-info.java");
    let java_path = PathBuf::from("/workspace/src/main/java/com/example/api/A.java");

    let java_text = "package com.example.api; class A {}".to_string();
    let module_text = "module my.mod { exports com.example.a<|> }";

    let (db, file, pos) = fixture_multi(module_path, module_text, vec![(java_path, java_text)]);

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.iter().any(|l| *l == "api" || *l == "api."),
        "expected module-info exports completion to contain api; got {labels:?}"
    );
}

#[test]
fn completion_in_module_info_exports_does_not_suggest_jdk_package_segments() {
    let module_path = PathBuf::from("/workspace/module-info.java");
    let java_path = PathBuf::from("/workspace/src/main/java/com/example/api/A.java");

    let java_text = "package com.example.api; class A {}".to_string();
    let module_text = "module my.mod { exports <|> }";

    let (db, file, pos) = fixture_multi(module_path, module_text, vec![(java_path, java_text)]);

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"com."),
        "expected exports completion to include workspace package segment `com.`; got {labels:?}"
    );
    assert!(
        !labels.contains(&"java.") && !labels.contains(&"java"),
        "did not expect exports completion to suggest `java` packages; got {labels:?}"
    );
}

#[test]
fn completion_in_module_info_exports_java_prefix_is_empty() {
    let module_path = PathBuf::from("/workspace/module-info.java");
    let java_path = PathBuf::from("/workspace/src/main/java/com/example/api/A.java");

    let java_text = "package com.example.api; class A {}".to_string();
    let module_text = "module my.mod { exports ja<|> }";

    let (db, file, pos) = fixture_multi(module_path, module_text, vec![(java_path, java_text)]);

    let items = completions(&db, file, pos);
    assert!(
        items.is_empty(),
        "expected no completions for `exports ja...` (should not suggest JDK packages); got {items:#?}"
    );
}

#[test]
fn completion_in_module_info_body_suggests_directive_snippets() {
    let module_path = PathBuf::from("/workspace/module-info.java");
    let (db, file, pos) = fixture_multi(module_path, "module my.mod { <|> }", vec![]);

    let items = completions(&db, file, pos);
    let item = items
        .iter()
        .find(|i| i.label == "requires")
        .expect("expected requires snippet completion");

    assert_eq!(
        item.insert_text_format,
        Some(InsertTextFormat::SNIPPET),
        "expected requires completion to be a snippet; got {item:#?}"
    );
    assert!(
        item.insert_text
            .as_deref()
            .is_some_and(|t| t.contains("requires ${1:module};")),
        "expected requires snippet to contain placeholder text; got {item:#?}"
    );
}

#[test]
fn completion_in_module_info_requires_suggests_modifiers() {
    let module_path = PathBuf::from("/workspace/module-info.java");
    let (db, file, pos) = fixture_multi(module_path, "module my.mod { requires <|> }", vec![]);

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"static"),
        "expected requires completion to suggest `static`; got {labels:?}"
    );
    assert!(
        labels.contains(&"transitive"),
        "expected requires completion to suggest `transitive`; got {labels:?}"
    );
}

#[test]
fn completion_in_module_info_exports_suggests_to_keyword() {
    let module_path = PathBuf::from("/workspace/module-info.java");
    let (db, file, pos) = fixture_multi(
        module_path,
        "module my.mod { exports com.example.api <|> }",
        vec![],
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"to"),
        "expected exports completion to suggest `to`; got {labels:?}"
    );
}

#[test]
fn completion_in_module_info_provides_suggests_with_keyword() {
    let module_path = PathBuf::from("/workspace/module-info.java");
    let (db, file, pos) = fixture_multi(
        module_path,
        "module my.mod { provides com.example.Service <|> }",
        vec![],
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"with"),
        "expected provides completion to suggest `with`; got {labels:?}"
    );
}

#[test]
fn completion_in_module_info_uses_suggests_workspace_service_type() {
    let module_path = PathBuf::from("/workspace/module-info.java");
    let service_path = PathBuf::from("/workspace/src/main/java/com/example/spi/MyService.java");
    let service_text = "package com.example.spi; public interface MyService {}".to_string();

    let (db, file, pos) = fixture_multi(
        module_path,
        "module my.mod { uses com.example.spi.My<|> }",
        vec![(service_path, service_text)],
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"MyService"),
        "expected uses completion to suggest workspace type MyService; got {labels:?}"
    );
}

#[test]
fn completion_in_module_info_provides_suggests_workspace_service_type_before_with() {
    let module_path = PathBuf::from("/workspace/module-info.java");
    let service_path = PathBuf::from("/workspace/src/main/java/com/example/spi/MyService.java");
    let service_text = "package com.example.spi; public interface MyService {}".to_string();

    let (db, file, pos) = fixture_multi(
        module_path,
        "module my.mod { provides com.example.spi.My<|> }",
        vec![(service_path, service_text)],
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"MyService"),
        "expected provides completion to suggest workspace type MyService; got {labels:?}"
    );
}

#[test]
fn completion_in_module_info_provides_suggests_workspace_impl_type_after_with() {
    let module_path = PathBuf::from("/workspace/module-info.java");
    let service_path = PathBuf::from("/workspace/src/main/java/com/example/spi/MyService.java");
    let impl_path = PathBuf::from("/workspace/src/main/java/com/example/impl/MyServiceImpl.java");

    let service_text = "package com.example.spi; public interface MyService {}".to_string();
    let impl_text =
        "package com.example.impl; public class MyServiceImpl implements com.example.spi.MyService {}"
            .to_string();

    let (db, file, pos) = fixture_multi(
        module_path,
        "module my.mod { provides com.example.spi.MyService with com.example.impl.My<|> }",
        vec![(service_path, service_text), (impl_path, impl_text)],
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"MyServiceImpl"),
        "expected provides completion to suggest impl type MyServiceImpl; got {labels:?}"
    );
}

#[test]
fn completion_new_expression_adds_import_edit_for_arraylist_without_imports() {
    let (db, file, pos) = fixture("class A { void m(){ new Arr<|> } }");

    let items = completions(&db, file, pos);
    let item = items
        .iter()
        .find(|i| i.label == "ArrayList")
        .expect("expected ArrayList completion item");

    let edits = item
        .additional_text_edits
        .as_ref()
        .expect("expected additional_text_edits for ArrayList completion");
    assert!(
        edits
            .iter()
            .any(|e| e.new_text == "import java.util.ArrayList;\n"),
        "expected import edit for java.util.ArrayList; got {edits:#?}"
    );

    let import_edit = edits
        .iter()
        .find(|e| e.new_text == "import java.util.ArrayList;\n")
        .expect("expected import edit for java.util.ArrayList");

    assert_eq!(import_edit.range.start, lsp_types::Position::new(0, 0));
    assert_eq!(import_edit.range.end, lsp_types::Position::new(0, 0));
}

#[test]
fn completion_new_expression_adds_import_after_package_declaration() {
    let (db, file, pos) = fixture("package com.foo;\nclass A { void m(){ new Arr<|> } }");

    let items = completions(&db, file, pos);
    let item = items
        .iter()
        .find(|i| i.label == "ArrayList")
        .expect("expected ArrayList completion item");

    let edits = item
        .additional_text_edits
        .as_ref()
        .expect("expected additional_text_edits for ArrayList completion");

    let import_edit = edits
        .iter()
        .find(|e| e.new_text == "import java.util.ArrayList;\n")
        .expect("expected import edit for java.util.ArrayList");

    assert_eq!(import_edit.range.start, lsp_types::Position::new(1, 0));
    assert_eq!(import_edit.range.end, lsp_types::Position::new(1, 0));
}

#[test]
fn completion_new_expression_adds_import_after_existing_imports() {
    let (db, file, pos) =
        fixture("package com.foo;\nimport java.util.List;\nclass A { void m(){ new Arr<|> } }");

    let items = completions(&db, file, pos);
    let item = items
        .iter()
        .find(|i| i.label == "ArrayList")
        .expect("expected ArrayList completion item");

    let edits = item
        .additional_text_edits
        .as_ref()
        .expect("expected additional_text_edits for ArrayList completion");

    let import_edit = edits
        .iter()
        .find(|e| e.new_text == "import java.util.ArrayList;\n")
        .expect("expected import edit for java.util.ArrayList");

    assert_eq!(import_edit.range.start, lsp_types::Position::new(2, 0));
    assert_eq!(import_edit.range.end, lsp_types::Position::new(2, 0));
}

#[test]
fn completion_new_expression_includes_string_without_imports() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    Object x = new Str<|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"String"),
        "expected completion list to contain String; got {labels:?}"
    );
}

#[test]
fn completion_type_position_includes_string_in_local_var_declaration() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    Str<|> s;
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"String"),
        "expected completion list to contain String; got {labels:?}"
    );
}

#[test]
fn completion_type_position_includes_arraylist_in_extends_clause() {
    let (db, file, pos) = fixture(
        r#"
import java.util.*;
class A extends Arr<|> {}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"ArrayList"),
        "expected completion list to contain ArrayList; got {labels:?}"
    );
}

#[test]
fn completion_type_position_in_implements_clause_prefers_interfaces() {
    let (db, file, pos) = fixture(
        r#"
import foo.Bar;
interface I {}
class C implements I {}
class A { class D implements <|> {} }
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"I"),
        "expected completion list to contain interface I; got {labels:?}"
    );

    let i_pos = items
        .iter()
        .position(|i| i.label == "I")
        .expect("expected I to be present");
    let first_class_pos = items
        .iter()
        .position(|i| i.kind == Some(lsp_types::CompletionItemKind::CLASS))
        .expect("expected at least one class completion item");

    assert!(
        i_pos < first_class_pos,
        "expected interface I to be ranked above class candidates; got {labels:?}"
    );
}

#[test]
fn completion_type_position_in_throws_clause_includes_exception_subtypes() {
    let (db, file, pos) = fixture(
        r#"
class Ex extends Exception {}
class A { void m() throws <|> {} }
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"Ex"),
        "expected completion list to contain Ex; got {labels:?}"
    );
}

#[test]
fn completion_in_catch_parameter_name_does_not_trigger_type_position_completion() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    try {
    } catch (RuntimeException e<|>) {
    }
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"else"),
        "expected general completion keyword `else` (not type-position completions); got {labels:?}"
    );
}

#[test]
fn completion_type_position_adds_import_edit_for_arraylist_without_imports() {
    let (db, file, pos) = fixture("class A { void m(){ Arr<|> xs; } }");

    let items = completions(&db, file, pos);
    let item = items
        .iter()
        .find(|i| i.label == "ArrayList")
        .expect("expected ArrayList completion item");

    let edits = item
        .additional_text_edits
        .as_ref()
        .expect("expected additional_text_edits for ArrayList completion");
    assert!(
        edits
            .iter()
            .any(|e| e.new_text == "import java.util.ArrayList;\n"),
        "expected import edit for java.util.ArrayList; got {edits:#?}"
    );

    let import_edit = edits
        .iter()
        .find(|e| e.new_text == "import java.util.ArrayList;\n")
        .expect("expected import edit for java.util.ArrayList");

    assert_eq!(import_edit.range.start, lsp_types::Position::new(0, 0));
    assert_eq!(import_edit.range.end, lsp_types::Position::new(0, 0));
}

#[test]
fn completion_type_position_adds_import_edit_for_workspace_type_without_imports() {
    let other_path = PathBuf::from("/workspace/src/main/java/p/FooBar.java");
    let main_path = PathBuf::from("/workspace/src/main/java/q/Main.java");

    let other_text = "package p; public class FooBar {}".to_string();
    let main_text = "package q;\nclass Main {\n  Foo<|> x;\n}\n";

    let (db, file, pos) = fixture_multi(main_path, main_text, vec![(other_path, other_text)]);

    let items = completions(&db, file, pos);
    let item = items
        .iter()
        .find(|i| i.label == "FooBar")
        .expect("expected FooBar completion item");

    let edits = item
        .additional_text_edits
        .as_ref()
        .expect("expected additional_text_edits for FooBar completion");
    assert!(
        edits.iter().any(|e| e.new_text == "import p.FooBar;\n"),
        "expected import edit for p.FooBar; got {edits:#?}"
    );

    let import_edit = edits
        .iter()
        .find(|e| e.new_text == "import p.FooBar;\n")
        .expect("expected import edit for p.FooBar");
    assert_eq!(import_edit.range.start, lsp_types::Position::new(1, 0));
    assert_eq!(import_edit.range.end, lsp_types::Position::new(1, 0));
}

#[test]
fn completion_includes_string_after_instanceof() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m(Object o) {
    if (o instanceof Str<|>) {}
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"String"),
        "expected completion list to contain String; got {labels:?}"
    );
}

#[test]
fn completion_instanceof_does_not_suggest_primitives() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m(Object o) {
    if (o instanceof Str<|>) {}
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        !labels.contains(&"int"),
        "expected completion list to not contain primitive int; got {labels:?}"
    );
}

#[test]
fn completion_type_position_includes_boolean_primitive() {
    let (db, file, pos) = fixture("class A { bo<|> x; }");

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"boolean"),
        "expected completion list to contain boolean; got {labels:?}"
    );
}

#[test]
fn completion_type_position_includes_int_primitive() {
    let (db, file, pos) = fixture("class A { in<|> x; }");

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"int"),
        "expected completion list to contain int; got {labels:?}"
    );
}

#[test]
fn completion_type_position_includes_var_keyword_in_local_var_declaration() {
    let (db, file, pos) = fixture("class A { void m(){ va<|> x = 1; } }");

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"var"),
        "expected completion list to contain var; got {labels:?}"
    );
}

#[test]
fn completion_includes_workspace_annotation_types_after_at_sign() {
    let anno_path = PathBuf::from("/workspace/src/main/java/p/MyAnno.java");
    let main_path = PathBuf::from("/workspace/src/main/java/p/Main.java");

    let anno_text = "package p; public @interface MyAnno {}".to_string();
    let main_text = r#"package p; @My<|> class Main {}"#;

    let (db, file, pos) = fixture_multi(main_path, main_text, vec![(anno_path, anno_text)]);

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"MyAnno"),
        "expected completion list to contain MyAnno; got {labels:?}"
    );

    let main_without_caret = main_text.replace("<|>", "");
    let at_my = main_without_caret
        .find("@My")
        .expect("expected @My prefix in fixture");
    let my_start = at_my + 1; // skip '@'

    let item = items
        .iter()
        .find(|i| i.label == "MyAnno")
        .expect("expected MyAnno completion item");
    let edit = match item.text_edit.as_ref().expect("expected text_edit") {
        CompletionTextEdit::Edit(edit) => edit,
        other => panic!("unexpected text_edit variant: {other:?}"),
    };

    assert_eq!(edit.new_text, "MyAnno");
    assert_eq!(
        edit.range.start,
        offset_to_position(&main_without_caret, my_start)
    );
    assert_eq!(edit.range.end, pos);
}

#[test]
fn completion_includes_math_static_members() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    Math.<|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"max"),
        "expected completion list to contain Math.max; got {labels:?}"
    );
    assert!(
        labels.contains(&"PI"),
        "expected completion list to contain Math.PI; got {labels:?}"
    );
}

#[test]
fn completion_includes_collections_static_members_with_auto_import() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    Collections.<|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let empty_list = items
        .iter()
        .find(|i| i.label == "emptyList")
        .expect("expected Collections.emptyList completion item");

    let edits = empty_list
        .additional_text_edits
        .as_ref()
        .expect("expected auto-import edit for java.util.Collections");
    assert!(
        edits
            .iter()
            .any(|e| e.new_text == "import java.util.Collections;\n"),
        "expected import edit for java.util.Collections; got {edits:#?}"
    );
}

#[test]
fn completion_includes_collections_static_method_snippet_placeholders_with_auto_import() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    Collections.si<|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let singleton = items
        .iter()
        .find(|i| i.label == "singletonList")
        .expect("expected Collections.singletonList completion item");

    assert_eq!(
        singleton.insert_text_format,
        Some(InsertTextFormat::SNIPPET),
        "expected singletonList to insert a snippet; got {singleton:#?}"
    );
    let edit = match singleton.text_edit.as_ref().expect("expected text_edit") {
        CompletionTextEdit::Edit(edit) => edit,
        other => panic!("unexpected text_edit variant: {other:?}"),
    };
    assert_eq!(edit.new_text, "singletonList(${1:arg0})$0");
}

#[test]
fn completion_includes_collections_zero_arg_method_without_snippet() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    Collections.em<|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let empty_list = items
        .iter()
        .find(|i| i.label == "emptyList")
        .expect("expected Collections.emptyList completion item");

    assert_eq!(
        empty_list.insert_text_format, None,
        "expected emptyList to be plain text (no snippet); got {empty_list:#?}"
    );
    let edit = match empty_list.text_edit.as_ref().expect("expected text_edit") {
        CompletionTextEdit::Edit(edit) => edit,
        other => panic!("unexpected text_edit variant: {other:?}"),
    };
    assert_eq!(edit.new_text, "emptyList()");
}

#[test]
fn completion_ranks_math_static_members_for_prefix() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    Math.ma<|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    assert!(
        !items.is_empty(),
        "expected non-empty completion list for Math.ma; got empty"
    );
    assert_eq!(
        items[0].label,
        "max",
        "expected Math.max to rank first for prefix 'ma'; got {:?}",
        items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>()
    );
}

#[test]
fn completion_includes_static_import_members() {
    let (db, file, pos) = fixture(
        r#"
import static java.lang.Math.ma<|>;
class A {}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"max"),
        "expected completion list to contain Math.max; got {labels:?}"
    );
    let max = items
        .iter()
        .find(|i| i.label == "max")
        .expect("expected max completion item");
    assert_eq!(
        max.kind,
        Some(lsp_types::CompletionItemKind::METHOD),
        "expected Math.max to be classified as a method; got {max:#?}"
    );
    assert!(
        !labels.contains(&"*"),
        "expected `*` to not be suggested while typing a member name; got {labels:?}"
    );
}

#[test]
fn completion_includes_static_import_star() {
    let (db, file, pos) = fixture(
        r#"
 import static java.lang.Math.<|>;
 class A {}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"*"),
        "expected completion list to contain `*`; got {labels:?}"
    );
    assert!(
        labels.contains(&"max"),
        "expected completion list to contain Math.max with empty member prefix; got {labels:?}"
    );
}

#[test]
fn completion_includes_static_import_nested_type() {
    let (db, file, pos) = fixture(
        r#"
import static java.util.Map.<|>;
class A {}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"Entry"),
        "expected completion list to contain Map.Entry; got {labels:?}"
    );
}

#[test]
fn completion_in_static_import_after_dot_includes_members() {
    let (db, file, pos) = fixture(
        r#"
import static java.lang.Math.<|>;
class A {}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"max"),
        "expected completion list to contain Math.max; got {labels:?}"
    );
}

#[test]
fn completion_includes_static_import_constant_kind() {
    let (db, file, pos) = fixture(
        r#"
import static java.lang.Math.P<|>;
class A {}
"#,
    );

    let items = completions(&db, file, pos);
    let pi = items
        .iter()
        .find(|i| i.label == "PI")
        .expect("expected PI completion item");
    assert_eq!(
        pi.kind,
        Some(lsp_types::CompletionItemKind::CONSTANT),
        "expected Math.PI to be classified as a constant; got {pi:#?}"
    );
}

#[test]
fn completion_includes_static_single_imported_member_in_expression() {
    let (db, file, pos) = fixture(
        r#"
import static java.lang.Math.max;

class A {
  void m() {
    ma<|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"max"),
        "expected completion list to contain statically imported `max`; got {labels:?}"
    );
}

#[test]
fn completion_includes_static_star_imported_constant_in_expression() {
    let (db, file, pos) = fixture(
        r#"
import static java.lang.Math.*;

class A {
  void m() {
    PI<|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"PI"),
        "expected completion list to contain statically imported `PI`; got {labels:?}"
    );
}

#[test]
fn completion_includes_static_imported_field_in_expression() {
    let (db, file, pos) = fixture(
        r#"
import static java.lang.System.out;

class A {
  void m() {
    ou<|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let out = items
        .iter()
        .find(|i| i.label == "out")
        .expect("expected out completion item");
    assert_eq!(
        out.kind,
        Some(lsp_types::CompletionItemKind::CONSTANT),
        "expected System.out to be classified as a constant; got {out:#?}"
    );
    assert_eq!(
        out.insert_text.as_deref(),
        Some("out"),
        "expected System.out to insert without parens; got {out:#?}"
    );
    assert_eq!(
        out.insert_text_format,
        None,
        "expected System.out to not use snippet insertion; got {out:#?}"
    );
}

#[test]
fn static_import_completion_replaces_only_member_segment() {
    let text_with_caret = r#"
 import static java.lang.Math.ma<|>;
 class A {}
"#;
    let (db, file, pos) = fixture(text_with_caret);

    let text = text_with_caret.replace("<|>", "");
    let member_start = text.find("Math.ma").expect("expected Math.ma in fixture") + "Math.".len();

    let items = completions(&db, file, pos);
    let item = items
        .iter()
        .find(|i| i.label == "max")
        .expect("expected max completion item");

    let edit = match item.text_edit.as_ref().expect("expected text_edit") {
        CompletionTextEdit::Edit(edit) => edit,
        other => panic!("unexpected text_edit variant: {other:?}"),
    };

    assert_eq!(edit.range.start, offset_to_position(&text, member_start));
    assert_eq!(edit.range.end, pos);
}

#[test]
fn completion_in_static_import_workspace_type_offers_star() {
    let outer_path = PathBuf::from("/workspace/src/main/java/p/Outer.java");
    let main_path = PathBuf::from("/workspace/src/main/java/q/Main.java");

    let outer_text = "package p; public class Outer { public static class Inner {} }".to_string();
    let main_text = r#"
package q;
import static p.Outer.<|>;
class A {}
"#;

    let (db, file, pos) = fixture_multi(main_path, main_text, vec![(outer_path, outer_text)]);
    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"*"),
        "expected completion list to contain `*` for workspace static import; got {labels:?}"
    );
}

#[test]
fn completion_in_generic_type_argument_includes_string() {
    let (db, file, pos) = fixture(
        r#"
import java.util.*;
class A {
  List<Str<|>> xs;
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"String"),
        "expected completion list to contain `String`; got {labels:?}"
    );
}

#[test]
fn completion_for_qualified_type_replaces_only_segment() {
    let (db, file, pos) = fixture(
        r#"
class A {
  java.util.Arr<|> xs;
}
"#,
    );

    let text_without_caret = db
        .file_text(file)
        .expect("expected file content for fixture")
        .to_string();

    let segment_start = text_without_caret
        .find("java.util.Arr")
        .expect("expected qualified name in fixture")
        + "java.util.".len();

    let items = completions(&db, file, pos);
    let item = items
        .iter()
        .find(|i| i.label == "ArrayList")
        .expect("expected ArrayList completion item");

    assert!(
        item.additional_text_edits
            .as_ref()
            .map_or(true, |edits| edits.is_empty()),
        "expected qualified type completion to avoid additional edits (imports); got: {:#?}",
        item.additional_text_edits
    );

    let edit = match item.text_edit.as_ref().expect("expected text_edit") {
        CompletionTextEdit::Edit(edit) => edit,
        other => panic!("unexpected text_edit variant: {other:?}"),
    };

    assert_eq!(
        edit.range.start,
        offset_to_position(&text_without_caret, segment_start),
        "expected completion to replace only `Arr` segment"
    );
    assert_eq!(edit.range.end, pos);
}

#[test]
fn completion_includes_postfix_if_for_boolean() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    boolean cond = true;
    cond.if<|>
  }
}
"#,
    );

    let text_without_caret = db
        .file_text(file)
        .expect("expected file content for fixture")
        .to_string();
    let cond_start = text_without_caret
        .find("cond.if")
        .expect("expected cond.if in fixture");

    let items = completions(&db, file, pos);
    let item = items
        .iter()
        .find(|i| i.label == "if" && i.kind == Some(lsp_types::CompletionItemKind::SNIPPET))
        .expect("expected postfix `if` snippet completion");
    let edit = match item.text_edit.as_ref().expect("expected text_edit") {
        CompletionTextEdit::Edit(edit) => edit,
        other => panic!("unexpected text_edit variant: {other:?}"),
    };

    assert_eq!(
        edit.range.start,
        offset_to_position(&text_without_caret, cond_start)
    );
    assert_eq!(edit.range.end, pos);
    assert!(
        edit.new_text.contains("if (cond)"),
        "expected snippet to contain `if (cond)`; got {:?}",
        edit.new_text
    );
}

#[test]
fn completion_includes_postfix_if_for_field_when_shadowed_out_of_scope() {
    let (db, file, pos) = fixture(
        r#"
class A {
  boolean cond = true;
  void m() {
    {
      String cond = "";
    }
    cond.if<|>
  }
}
"#,
    );

    let text_without_caret = db
        .file_text(file)
        .expect("expected file content for fixture")
        .to_string();
    let cond_start = text_without_caret
        .rfind("cond.if")
        .expect("expected cond.if in fixture");

    let items = completions(&db, file, pos);
    let item = items
        .iter()
        .find(|i| i.label == "if" && i.kind == Some(lsp_types::CompletionItemKind::SNIPPET))
        .expect("expected postfix `if` snippet completion");

    let edit = match item.text_edit.as_ref().expect("expected text_edit") {
        CompletionTextEdit::Edit(edit) => edit,
        other => panic!("unexpected text_edit variant: {other:?}"),
    };

    assert_eq!(
        edit.range.start,
        offset_to_position(&text_without_caret, cond_start)
    );
    assert_eq!(edit.range.end, pos);
    assert!(
        edit.new_text.contains("if (cond)"),
        "expected snippet to contain `if (cond)`; got {:?}",
        edit.new_text
    );
}

#[test]
fn completion_includes_postfix_nn_for_reference() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    String s = "";
    s.nn<|>
  }
}
"#,
    );

    let text_without_caret = db
        .file_text(file)
        .expect("expected file content for fixture")
        .to_string();
    let expr_start = text_without_caret
        .find("s.nn")
        .expect("expected s.nn in fixture");

    let items = completions(&db, file, pos);
    let item = items
        .iter()
        .find(|i| i.label == "nn" && i.kind == Some(lsp_types::CompletionItemKind::SNIPPET))
        .expect("expected postfix `nn` snippet completion");

    let edit = match item.text_edit.as_ref().expect("expected text_edit") {
        CompletionTextEdit::Edit(edit) => edit,
        other => panic!("unexpected text_edit variant: {other:?}"),
    };

    assert_eq!(
        edit.range.start,
        offset_to_position(&text_without_caret, expr_start)
    );
    assert_eq!(edit.range.end, pos);
    assert!(
        edit.new_text.contains("if (s != null)"),
        "expected snippet to contain `if (s != null)`; got {:?}",
        edit.new_text
    );
}

#[test]
fn completion_includes_postfix_for_this_qualified_receiver() {
    let (db, file, pos) = fixture(
        r#"
class A {
  String s = "";
  void m() {
    this.s.nn<|>
  }
}
"#,
    );

    let text_without_caret = db
        .file_text(file)
        .expect("expected file content for fixture")
        .to_string();
    let expr_start = text_without_caret
        .find("this.s.nn")
        .expect("expected this.s.nn in fixture");

    let items = completions(&db, file, pos);
    let item = items
        .iter()
        .find(|i| i.label == "nn" && i.kind == Some(lsp_types::CompletionItemKind::SNIPPET))
        .expect("expected postfix `nn` snippet completion");

    let edit = match item.text_edit.as_ref().expect("expected text_edit") {
        CompletionTextEdit::Edit(edit) => edit,
        other => panic!("unexpected text_edit variant: {other:?}"),
    };

    assert_eq!(
        edit.range.start,
        offset_to_position(&text_without_caret, expr_start)
    );
    assert_eq!(edit.range.end, pos);
    assert!(
        edit.new_text.contains("if (this.s != null)"),
        "expected snippet to contain `if (this.s != null)`; got {:?}",
        edit.new_text
    );
}

#[test]
fn completion_does_not_offer_postfix_for_non_this_qualified_receiver() {
    let (db, file, pos) = fixture(
        r#"
class A {
  String s = "";
  void m(A other) {
    other.s.nn<|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    assert!(
        !items
            .iter()
            .any(|i| { i.label == "nn" && i.kind == Some(lsp_types::CompletionItemKind::SNIPPET) }),
        "expected no postfix `nn` snippet for non-this qualified receiver; got {items:#?}"
    );
}

#[test]
fn completion_falls_back_when_member_receiver_type_is_unknown() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    this.missing.n<|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    assert!(
        !items.is_empty(),
        "expected completion list to be non-empty even when member receiver type can't be inferred; got {items:#?}"
    );
}

#[test]
fn completion_includes_postfix_for_for_array() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    int[] xs = null;
    xs.for<|>
  }
}
"#,
    );

    let text_without_caret = db
        .file_text(file)
        .expect("expected file content for fixture")
        .to_string();
    let expr_start = text_without_caret
        .find("xs.for")
        .expect("expected xs.for in fixture");

    let items = completions(&db, file, pos);
    let item = items
        .iter()
        .find(|i| i.label == "for" && i.kind == Some(lsp_types::CompletionItemKind::SNIPPET))
        .expect("expected postfix `for` snippet completion");

    let edit = match item.text_edit.as_ref().expect("expected text_edit") {
        CompletionTextEdit::Edit(edit) => edit,
        other => panic!("unexpected text_edit variant: {other:?}"),
    };

    assert_eq!(
        edit.range.start,
        offset_to_position(&text_without_caret, expr_start)
    );
    assert_eq!(edit.range.end, pos);
    assert!(
        edit.new_text.contains("for (int"),
        "expected snippet to contain `for (int`; got {:?}",
        edit.new_text
    );
}

#[test]
fn completion_includes_postfix_stream_for_list() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    List l = null;
    l.stream<|>
  }
}
"#,
    );

    let text_without_caret = db
        .file_text(file)
        .expect("expected file content for fixture")
        .to_string();
    let expr_start = text_without_caret
        .find("l.stream")
        .expect("expected l.stream in fixture");

    let items = completions(&db, file, pos);
    let item = items
        .iter()
        .find(|i| i.label == "stream" && i.kind == Some(lsp_types::CompletionItemKind::SNIPPET))
        .expect("expected postfix `stream` snippet completion");

    let edit = match item.text_edit.as_ref().expect("expected text_edit") {
        CompletionTextEdit::Edit(edit) => edit,
        other => panic!("unexpected text_edit variant: {other:?}"),
    };

    assert_eq!(
        edit.range.start,
        offset_to_position(&text_without_caret, expr_start)
    );
    assert_eq!(edit.range.end, pos);
    assert!(
        edit.new_text.contains(".stream()"),
        "expected snippet to contain `.stream()`; got {:?}",
        edit.new_text
    );
}

#[test]
fn completion_includes_postfix_sout() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    String s = "";
    s.sout<|>
  }
}
"#,
    );

    let text_without_caret = db
        .file_text(file)
        .expect("expected file content for fixture")
        .to_string();
    let expr_start = text_without_caret
        .find("s.sout")
        .expect("expected s.sout in fixture");

    let items = completions(&db, file, pos);
    let item = items
        .iter()
        .find(|i| i.label == "sout" && i.kind == Some(lsp_types::CompletionItemKind::SNIPPET))
        .expect("expected postfix `sout` snippet completion");

    let edit = match item.text_edit.as_ref().expect("expected text_edit") {
        CompletionTextEdit::Edit(edit) => edit,
        other => panic!("unexpected text_edit variant: {other:?}"),
    };

    assert_eq!(
        edit.range.start,
        offset_to_position(&text_without_caret, expr_start)
    );
    assert_eq!(edit.range.end, pos);
    assert!(
        edit.new_text.contains("System.out.println"),
        "expected snippet to contain `System.out.println`; got {:?}",
        edit.new_text
    );
}

#[test]
fn completion_in_import_offers_package_segment_and_replaces_only_segment() {
    let (db, file, pos) = fixture(
        r#"
import java.u<|>;
class A {}
"#,
    );

    let items = completions(&db, file, pos);
    let item = items
        .iter()
        .find(|i| i.label == "util" || i.label == "util.")
        .expect("expected java.util package completion");

    let text_without_caret = r#"
import java.u;
class A {}
"#;

    let u_offset = text_without_caret
        .find("java.u")
        .expect("expected java.u in fixture")
        + "java.".len();

    let edit = match item.text_edit.as_ref().expect("expected text_edit") {
        CompletionTextEdit::Edit(edit) => edit,
        other => panic!("unexpected text_edit variant: {other:?}"),
    };

    assert_eq!(
        edit.range.start,
        offset_to_position(&text_without_caret, u_offset)
    );
    assert_eq!(edit.range.end, pos);
}

#[test]
fn completion_in_static_import_type_segment_includes_entry() {
    let text_with_caret = r#"
import static java.util.Map.E<|>;
class A {}
"#;
    let (db, file, pos) = fixture(text_with_caret);

    let items = completions(&db, file, pos);
    let item = items
        .iter()
        .find(|i| i.label == "Entry")
        .expect("expected java.util.Map.Entry nested type completion in static import");

    let text = text_with_caret.replace("<|>", "");
    let segment_start = text.find("Map.E").expect("expected Map.E in fixture") + "Map.".len();

    let edit = match item.text_edit.as_ref().expect("expected text_edit") {
        CompletionTextEdit::Edit(edit) => edit,
        other => panic!("unexpected text_edit variant: {other:?}"),
    };

    assert_eq!(edit.range.start, offset_to_position(&text, segment_start));
    assert_eq!(edit.range.end, pos);
}

#[test]
fn completion_in_import_includes_jdk_type() {
    let (db, file, pos) = fixture(
        r#"
 import java.util.<|>;
 class A {}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"List"),
        "expected completion list to contain java.util.List; got {labels:?}"
    );
}

#[test]
fn completion_in_import_after_type_prefix_includes_star() {
    let (db, file, pos) = fixture(
        r#"
import java.util.Map.<|>;
class A {}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"*"),
        "expected completion list to contain `*` after type prefix; got {labels:?}"
    );
}

#[test]
fn completion_in_import_nested_type_segment_includes_entry() {
    let text_with_caret = r#"
import java.util.Map.E<|>;
class A {}
"#;
    let (db, file, pos) = fixture(text_with_caret);

    let items = completions(&db, file, pos);
    let item = items
        .iter()
        .find(|i| i.label == "Entry")
        .expect("expected java.util.Map.Entry nested type completion");

    let text = text_with_caret.replace("<|>", "");
    let segment_start = text.find("Map.E").expect("expected Map.E in fixture") + "Map.".len();

    let edit = match item.text_edit.as_ref().expect("expected text_edit") {
        CompletionTextEdit::Edit(edit) => edit,
        other => panic!("unexpected text_edit variant: {other:?}"),
    };

    assert_eq!(edit.range.start, offset_to_position(&text, segment_start));
    assert_eq!(edit.range.end, pos);
}

#[test]
fn completion_in_import_nested_type_segment_with_whitespace_includes_entry() {
    let text_with_caret = r#"
import java . util . Map . E<|>;
class A {}
"#;
    let (db, file, pos) = fixture(text_with_caret);

    let items = completions(&db, file, pos);
    let item = items
        .iter()
        .find(|i| i.label == "Entry")
        .expect("expected java.util.Map.Entry nested type completion");

    let text = text_with_caret.replace("<|>", "");
    let segment_start = text
        .find("Map . E")
        .expect("expected Map . E in fixture")
        + "Map . ".len();

    let edit = match item.text_edit.as_ref().expect("expected text_edit") {
        CompletionTextEdit::Edit(edit) => edit,
        other => panic!("unexpected text_edit variant: {other:?}"),
    };

    assert_eq!(edit.range.start, offset_to_position(&text, segment_start));
    assert_eq!(edit.range.end, pos);
}

#[test]
fn completion_type_position_nested_type_includes_entry() {
    let (db, file, pos) = fixture(
        r#"
 import java.util.Map;
 class A { Map.En<|> e; }
 "#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"Entry"),
        "expected type-position completion list to contain Map.Entry; got {labels:?}"
    );
}

#[test]
fn completion_in_import_workspace_nested_type_segment_includes_inner() {
    let outer_path = PathBuf::from("/workspace/src/main/java/p/Outer.java");
    let main_path = PathBuf::from("/workspace/src/main/java/q/Main.java");

    let outer_text = "package p; public class Outer { public static class Inner {} }".to_string();
    let text_with_caret = r#"
package q;
import p.Outer.I<|>;
class A {}
"#;

    let (db, file, pos) = fixture_multi(main_path, text_with_caret, vec![(outer_path, outer_text)]);

    let items = completions(&db, file, pos);
    let item = items
        .iter()
        .find(|i| i.label == "Inner")
        .expect("expected p.Outer.Inner nested type completion");

    let text = text_with_caret.replace("<|>", "");
    let segment_start = text.find("Outer.I").expect("expected Outer.I in fixture") + "Outer.".len();

    let edit = match item.text_edit.as_ref().expect("expected text_edit") {
        CompletionTextEdit::Edit(edit) => edit,
        other => panic!("unexpected text_edit variant: {other:?}"),
    };

    assert_eq!(edit.range.start, offset_to_position(&text, segment_start));
    assert_eq!(edit.range.end, pos);
}

#[test]
fn completion_type_position_nested_type_with_star_import_includes_entry() {
    let (db, file, pos) = fixture(
        r#"
import java.util.*;
class A { Map.En<|> e; }
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"Entry"),
        "expected type-position completion list to contain Map.Entry via star import; got {labels:?}"
    );
}

#[test]
fn completion_static_member_on_fully_qualified_type_with_whitespace_includes_empty_list() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void f() {
    java . util . Collections . empt<|>();
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"emptyList"),
        "expected completion list to contain java.util.Collections.emptyList even with whitespace around '.'; got {labels:?}"
    );
}

#[test]
fn completion_type_position_nested_type_with_whitespace_around_dot_includes_entry() {
    let (db, file, pos) = fixture(
        r#"
import java.util.Map;
class A { Map . En<|> e; }
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"Entry"),
        "expected type-position completion list to contain Map.Entry even with whitespace around '.'; got {labels:?}"
    );
}

#[test]
fn completion_type_position_nested_type_with_whitespace_in_qualified_name_includes_entry() {
    let (db, file, pos) = fixture(
        r#"
class A { java . util . Map . En<|> e; }
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"Entry"),
        "expected type-position completion list to contain java.util.Map.Entry even with whitespace in qualified name; got {labels:?}"
    );
}

#[test]
fn completion_type_position_workspace_nested_type_includes_inner() {
    let (db, file, pos) = fixture(
        r#"
package p;

class Outer {
  static class Inner {}
}

class A { Outer.In<|> x; }
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"Inner"),
        "expected type-position completion list to contain Outer.Inner; got {labels:?}"
    );
}

#[test]
fn completion_type_position_imported_workspace_nested_type_includes_deep() {
    let (db, file, pos) = fixture_multi(
        PathBuf::from("/workspace/src/main/java/a/A.java"),
        r#"
package a;

import p.Outer.Inner;

class A { Inner.De<|> x; }
"#,
        vec![(
            PathBuf::from("/workspace/src/main/java/p/Outer.java"),
            r#"
package p;

public class Outer {
  public static class Inner {
    public static class Deep {}
  }
}
"#
            .to_string(),
        )],
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"Deep"),
        "expected type-position completion list to contain Inner.Deep; got {labels:?}"
    );
}

#[test]
fn completion_inside_line_comment_is_empty() {
    let (db, file, pos) = fixture("class A { // if<|>\n }");
    let items = completions(&db, file, pos);
    assert!(
        items.is_empty(),
        "expected no completions inside line comment; got {items:#?}"
    );
    assert!(
        !items.iter().any(|i| i.label == "if"),
        "expected no keyword completions inside line comment; got {items:#?}"
    );
}

#[test]
fn completion_suppressed_in_block_comment() {
    let (db, file, pos) = fixture("/* ret<|> */");
    let items = completions(&db, file, pos);
    assert!(items.is_empty(), "expected no completions; got {items:#?}");
}

#[test]
fn completion_inside_block_comment_is_empty() {
    let (db, file, pos) = fixture("class A { /* ret<|> */ }");
    let items = completions(&db, file, pos);
    assert!(
        items.is_empty(),
        "expected no completions inside block comment; got {items:#?}"
    );
}

#[test]
fn completion_inside_block_comment_import_is_empty() {
    let (db, file, pos) = fixture("/*\nimport java.util.Arr<|>\n*/");
    let items = completions(&db, file, pos);
    assert!(
        items.is_empty(),
        "expected no completions inside commented-out import; got {items:#?}"
    );
}

#[test]
fn completion_inside_string_literal_is_empty() {
    let (db, file, pos) = fixture(r#"class A { void m(){ String s = "ret<|>"; } }"#);
    let items = completions(&db, file, pos);
    assert!(
        items.is_empty(),
        "expected no completions inside string literal; got {items:#?}"
    );
}

#[test]
fn completion_is_suppressed_inside_string_literal() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    String s = "hel<|>lo";
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        items.is_empty(),
        "expected no completions inside a normal string literal; got {labels:?}"
    );
    assert!(
        !labels.contains(&"if"),
        "expected completion list to not contain Java keywords like `if`; got {labels:?}"
    );
}

#[test]
fn completion_inside_string_literal_escape_sequence_suggests_escapes() {
    let (db, file, pos) = fixture(r#"class A { void m(){ String s = "\n<|>"; } }"#);
    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&r#"\n"#),
        "expected escape completions (e.g. `\\\\n`) inside string literal; got {labels:?}"
    );
    assert!(
        !labels.contains(&"if"),
        "expected completion list to not contain Java keywords like `if`; got {labels:?}"
    );
}

#[test]
fn completion_inside_string_literal_unicode_escape_sequence_suggests_unicode_snippet() {
    let (db, file, pos) = fixture(r#"class A { void m(){ String s = "\u<|>"; } }"#);
    let items = completions(&db, file, pos);

    let unicode = items
        .iter()
        .find(|i| i.label == r#"\u0000"#)
        .unwrap_or_else(|| {
            panic!(
                "expected unicode escape completion inside string literal; got labels {:?}",
                items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>()
            )
        });

    assert_eq!(unicode.insert_text.as_deref(), Some(r#"\u${1:0000}"#));
    assert_eq!(unicode.insert_text_format, Some(InsertTextFormat::SNIPPET));
}

#[test]
fn completion_inside_char_literal_is_empty() {
    let (db, file, pos) = fixture("class A { void m(){ char c = 'a<|>'; } }");
    let items = completions(&db, file, pos);
    assert!(
        items.is_empty(),
        "expected no completions inside character literal; got {items:#?}"
    );
}

#[test]
fn completion_inside_unterminated_text_block_at_eof_is_empty() {
    // Regression test: ensure we still suppress completions when the user has typed a partial text
    // block closing delimiter (e.g. `""` at EOF), which the lexer produces as an `Error` token.
    let (db, file, pos) = fixture(r#"class A { void m(){ String s = """hello""<|>"#);
    let items = completions(&db, file, pos);
    assert!(
        items.is_empty(),
        "expected no completions inside unterminated text block at EOF; got {items:#?}"
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
fn goto_definition_finds_local_variable() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    int x;
    x<|> = 1;
  }
}
"#,
    );

    let loc = goto_definition(&db, file, pos).expect("expected definition location");
    assert_eq!(loc.range.start.line, 3);
}

#[test]
fn goto_definition_finds_parameter() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m(int x) {
    x<|> = 1;
  }
}
"#,
    );

    let loc = goto_definition(&db, file, pos).expect("expected definition location");
    assert_eq!(loc.range.start.line, 2);
}

#[test]
fn goto_definition_finds_field() {
    let (db, file, pos) = fixture(
        r#"
class A {
  int f;
  void m() { f<|> = 1; }
}
"#,
    );

    let loc = goto_definition(&db, file, pos).expect("expected definition location");
    assert_eq!(loc.range.start.line, 2);
}

#[test]
fn goto_definition_resolves_type_across_files() {
    let main_path = PathBuf::from("/workspace/src/main/java/Main.java");
    let foo_path = PathBuf::from("/workspace/src/main/java/Foo.java");

    let main_text = r#"
class Main {
  void m() {
    Foo<|> x;
  }
}
"#;
    let foo_text = r#"
class Foo {}
"#
    .to_string();

    let (db, file, pos) = fixture_multi(main_path, main_text, vec![(foo_path, foo_text)]);

    let loc = goto_definition(&db, file, pos).expect("expected definition location");
    assert!(
        loc.uri.as_str().contains("Foo.java"),
        "expected goto-definition to resolve to Foo.java; got {:?}",
        loc.uri
    );
}

#[test]
fn goto_definition_resolves_workspace_type_in_import() {
    let main_path = PathBuf::from("/workspace/src/main/java/Main.java");
    let foo_path = PathBuf::from("/workspace/src/main/java/Foo.java");

    let main_text = r#"
import p.Foo<|>;
class Main { }
"#;
    let foo_text = "package p; class Foo {}".to_string();

    let (db, file, pos) = fixture_multi(main_path, main_text, vec![(foo_path, foo_text)]);

    let loc = goto_definition(&db, file, pos).expect("expected definition location");
    assert!(
        loc.uri.as_str().contains("Foo.java"),
        "expected goto-definition to resolve to Foo.java; got {:?}",
        loc.uri
    );
}

#[test]
fn goto_definition_resolves_qualified_type_usage() {
    let main_path = PathBuf::from("/workspace/src/main/java/Main.java");
    let foo_path = PathBuf::from("/workspace/src/main/java/Foo.java");

    let main_text = r#"
class Main { p.Foo<|> x; }
"#;
    let foo_text = "package p; class Foo {}".to_string();

    let (db, file, pos) = fixture_multi(main_path, main_text, vec![(foo_path, foo_text)]);

    let loc = goto_definition(&db, file, pos).expect("expected definition location");
    assert!(
        loc.uri.as_str().contains("Foo.java"),
        "expected goto-definition to resolve to Foo.java; got {:?}",
        loc.uri
    );
}

#[test]
fn goto_definition_resolves_enum_type_across_files() {
    let main_path = PathBuf::from("/workspace/src/main/java/Main.java");
    let color_path = PathBuf::from("/workspace/src/main/java/Color.java");

    let main_text = r#"
class Main {
  Color<|> c;
}
"#;
    let color_text = r#"
enum Color { RED }
"#
    .to_string();

    let (db, file, pos) = fixture_multi(main_path, main_text, vec![(color_path, color_text)]);

    let loc = goto_definition(&db, file, pos).expect("expected definition location");
    assert!(
        loc.uri.as_str().contains("Color.java"),
        "expected goto-definition to resolve to Color.java; got {:?}",
        loc.uri
    );
}

#[test]
fn goto_definition_resolves_record_type_across_files() {
    let main_path = PathBuf::from("/workspace/src/main/java/Main.java");
    let point_path = PathBuf::from("/workspace/src/main/java/Point.java");

    let main_text = r#"
class Main {
  Point<|> p;
}
"#;
    let point_text = r#"
record Point(int x, int y) {}
"#
    .to_string();

    let (db, file, pos) = fixture_multi(main_path, main_text, vec![(point_path, point_text)]);

    let loc = goto_definition(&db, file, pos).expect("expected definition location");
    assert!(
        loc.uri.as_str().contains("Point.java"),
        "expected goto-definition to resolve to Point.java; got {:?}",
        loc.uri
    );
}

#[test]
fn goto_definition_resolves_constructor_call_type_across_files() {
    let main_path = PathBuf::from("/workspace/src/main/java/Main.java");
    let foo_path = PathBuf::from("/workspace/src/main/java/Foo.java");

    let main_text = r#"
class Main {
  void m() {
    new Fo<|>o();
  }
}
"#;
    let foo_text = r#"
class Foo {}
"#
    .to_string();

    let (db, file, pos) = fixture_multi(main_path, main_text, vec![(foo_path, foo_text)]);

    let loc = goto_definition(&db, file, pos).expect("expected definition location");
    assert!(
        loc.uri.as_str().contains("Foo.java"),
        "expected goto-definition to resolve to Foo.java; got {:?}",
        loc.uri
    );
}

#[test]
fn goto_definition_resolves_constructor_call_type_with_block_comment_across_files() {
    let main_path = PathBuf::from("/workspace/src/main/java/Main.java");
    let foo_path = PathBuf::from("/workspace/src/main/java/Foo.java");

    let main_text = r#"
class Main {
  void m() {
    new /*comment*/ Fo<|>o();
  }
}
"#;
    let foo_text = r#"
class Foo {}
"#
    .to_string();

    let (db, file, pos) = fixture_multi(main_path, main_text, vec![(foo_path, foo_text)]);

    let loc = goto_definition(&db, file, pos).expect("expected definition location");
    assert!(
        loc.uri.as_str().contains("Foo.java"),
        "expected goto-definition to resolve to Foo.java; got {:?}",
        loc.uri
    );
}

#[test]
fn goto_definition_resolves_qualified_constructor_call_type_across_files() {
    let main_path = PathBuf::from("/workspace/src/main/java/Main.java");
    let foo_path = PathBuf::from("/workspace/src/main/java/p/Foo.java");

    let main_text = r#"
class Main {
  void m() {
    new p.Fo<|>o();
  }
}
"#;
    let foo_text = "package p; class Foo {}".to_string();

    let (db, file, pos) = fixture_multi(main_path, main_text, vec![(foo_path, foo_text)]);

    let loc = goto_definition(&db, file, pos).expect("expected definition location");
    assert!(
        loc.uri.as_str().contains("Foo.java"),
        "expected goto-definition to resolve to Foo.java; got {:?}",
        loc.uri
    );
}

#[test]
fn goto_definition_resolves_qualified_constructor_call_type_with_block_comment_across_files() {
    let main_path = PathBuf::from("/workspace/src/main/java/Main.java");
    let foo_path = PathBuf::from("/workspace/src/main/java/p/Foo.java");

    let main_text = r#"
class Main {
  void m() {
    new p/*comment*/.Fo<|>o();
  }
}
"#;
    let foo_text = "package p; class Foo {}".to_string();

    let (db, file, pos) = fixture_multi(main_path, main_text, vec![(foo_path, foo_text)]);

    let loc = goto_definition(&db, file, pos).expect("expected definition location");
    assert!(
        loc.uri.as_str().contains("Foo.java"),
        "expected goto-definition to resolve to Foo.java; got {:?}",
        loc.uri
    );
}

#[test]
fn goto_definition_resolves_member_method_call_across_files() {
    let main_path = PathBuf::from("/workspace/src/main/java/Main.java");
    let foo_path = PathBuf::from("/workspace/src/main/java/Foo.java");

    let main_text = r#"
class Main {
  void m() {
    Foo foo = new Foo();
    foo.ba<|>r();
  }
}
"#;
    let foo_text = r#"
class Foo {
  void bar() {}
}
"#
    .to_string();

    let (db, file, pos) = fixture_multi(main_path, main_text, vec![(foo_path, foo_text)]);

    let loc = goto_definition(&db, file, pos).expect("expected definition location");
    assert!(
        loc.uri.as_str().contains("Foo.java"),
        "expected goto-definition to resolve to Foo.java; got {:?}",
        loc.uri
    );
    assert_eq!(loc.range.start.line, 2);
}

#[test]
fn goto_definition_resolves_member_method_call_on_parameter_across_files() {
    let main_path = PathBuf::from("/workspace/src/main/java/Main.java");
    let foo_path = PathBuf::from("/workspace/src/main/java/Foo.java");

    let main_text = r#"
class Main {
  void m(Foo foo) {
    foo.ba<|>r();
  }
}
"#;
    let foo_text = r#"
class Foo {
  void bar() {}
}
"#
    .to_string();

    let (db, file, pos) = fixture_multi(main_path, main_text, vec![(foo_path, foo_text)]);

    let loc = goto_definition(&db, file, pos).expect("expected definition location");
    assert!(
        loc.uri.as_str().contains("Foo.java"),
        "expected goto-definition to resolve to Foo.java; got {:?}",
        loc.uri
    );
    assert_eq!(loc.range.start.line, 2);
}

#[test]
fn goto_definition_resolves_default_method_inherited_from_interface() {
    let main_path = PathBuf::from("/workspace/src/main/java/Main.java");
    let i_path = PathBuf::from("/workspace/src/main/java/I.java");
    let a_path = PathBuf::from("/workspace/src/main/java/A.java");

    let main_text = r#"
class Main {
  void m() {
    A a = new A();
    a.fo<|>o();
  }
}
"#;
    let i_text = r#"
interface I {
  default void foo() {}
}
"#
    .to_string();
    let a_text = r#"
class A implements I {}
"#
    .to_string();

    let (db, file, pos) = fixture_multi(
        main_path,
        main_text,
        vec![(i_path, i_text), (a_path, a_text)],
    );

    let loc = goto_definition(&db, file, pos).expect("expected definition location");
    assert!(
        loc.uri.as_str().contains("I.java"),
        "expected goto-definition to resolve to I.java; got {:?}",
        loc.uri
    );
    assert_eq!(loc.range.start.line, 2);
}

#[test]
fn goto_definition_resolves_interface_constant_inherited_by_class() {
    let main_path = PathBuf::from("/workspace/src/main/java/Main.java");
    let i_path = PathBuf::from("/workspace/src/main/java/I.java");
    let a_path = PathBuf::from("/workspace/src/main/java/A.java");

    let main_text = r#"
class Main {
  void m() {
    int y = A.<|>X;
  }
}
"#;
    let i_text = r#"
interface I {
  int X = 1;
}
"#
    .to_string();
    let a_text = r#"
class A implements I {}
"#
    .to_string();

    let (db, file, pos) = fixture_multi(
        main_path,
        main_text,
        vec![(i_path, i_text), (a_path, a_text)],
    );

    let loc = goto_definition(&db, file, pos).expect("expected definition location");
    assert!(
        loc.uri.as_str().contains("I.java"),
        "expected goto-definition to resolve to I.java; got {:?}",
        loc.uri
    );
    assert_eq!(loc.range.start.line, 2);
}

#[test]
fn goto_definition_resolves_interface_method_through_extends() {
    let main_path = PathBuf::from("/workspace/src/main/java/Main.java");
    let i_path = PathBuf::from("/workspace/src/main/java/I.java");
    let j_path = PathBuf::from("/workspace/src/main/java/J.java");

    let main_text = r#"
class Main {
  void m(J j) {
    j.fo<|>o();
  }
}
"#;
    let i_text = r#"
interface I {
  void foo();
}
"#
    .to_string();
    let j_text = r#"
interface J extends I {}
"#
    .to_string();

    let (db, file, pos) = fixture_multi(
        main_path,
        main_text,
        vec![(i_path, i_text), (j_path, j_text)],
    );

    let loc = goto_definition(&db, file, pos).expect("expected definition location");
    assert!(
        loc.uri.as_str().contains("I.java"),
        "expected goto-definition to resolve to I.java; got {:?}",
        loc.uri
    );
    assert_eq!(loc.range.start.line, 2);
}

#[test]
fn goto_definition_resolves_superinterface_method_declaration_across_files() {
    let main_path = PathBuf::from("/workspace/src/main/java/Main.java");
    let i0_path = PathBuf::from("/workspace/src/main/java/I0.java");
    let i1_path = PathBuf::from("/workspace/src/main/java/I1.java");

    let main_text = r#"
class Main {
  void m() {
    I1 i = null;
    i.fo<|>o();
  }
}
"#;
    let i0_text = r#"
interface I0 {
  void foo();
}
"#
    .to_string();
    let i1_text = r#"
interface I1 extends I0 {}
"#
    .to_string();

    let (db, file, pos) = fixture_multi(
        main_path,
        main_text,
        vec![(i0_path, i0_text), (i1_path, i1_text)],
    );

    let loc = goto_definition(&db, file, pos).expect("expected definition location");
    assert!(
        loc.uri.as_str().contains("I0.java"),
        "expected goto-definition to resolve to I0.java; got {:?}",
        loc.uri
    );
    assert_eq!(loc.range.start.line, 2);
}

#[test]
fn goto_definition_resolves_member_call_with_generic_typed_local() {
    let main_path = PathBuf::from("/workspace/src/main/java/Main.java");
    let foo_path = PathBuf::from("/workspace/src/main/java/Foo.java");

    let main_text = r#"
class Main {
  void m() {
    Foo<String> foo = new Foo();
    foo.ba<|>r();
  }
}
"#;
    let foo_text = r#"
class Foo<T> {
  void bar() {}
}
"#
    .to_string();

    let (db, file, pos) = fixture_multi(main_path, main_text, vec![(foo_path, foo_text)]);

    let loc = goto_definition(&db, file, pos).expect("expected definition location");
    assert!(
        loc.uri.as_str().contains("Foo.java"),
        "expected goto-definition to resolve to Foo.java; got {:?}",
        loc.uri
    );
    assert_eq!(loc.range.start.line, 2);
}

#[test]
fn goto_definition_resolves_generic_member_method_call_across_files() {
    let main_path = PathBuf::from("/workspace/src/main/java/Main.java");
    let foo_path = PathBuf::from("/workspace/src/main/java/Foo.java");

    let main_text = r#"
class Main {
  void m() {
    Foo foo = new Foo();
    foo.<String>ba<|>r();
  }
}
"#;
    let foo_text = r#"
class Foo {
  void bar() {}
}
"#
    .to_string();

    let (db, file, pos) = fixture_multi(main_path, main_text, vec![(foo_path, foo_text)]);

    let loc = goto_definition(&db, file, pos).expect("expected definition location");
    assert!(
        loc.uri.as_str().contains("Foo.java"),
        "expected goto-definition to resolve to Foo.java; got {:?}",
        loc.uri
    );
    assert_eq!(loc.range.start.line, 2);
}

#[test]
fn goto_definition_resolves_member_field_access_across_files() {
    let main_path = PathBuf::from("/workspace/src/main/java/Main.java");
    let foo_path = PathBuf::from("/workspace/src/main/java/Foo.java");

    let main_text = r#"
class Main {
  void m() {
    Foo foo = new Foo();
    int x = foo.va<|>l;
  }
}
"#;
    let foo_text = r#"
class Foo {
  int val;
}
"#
    .to_string();

    let (db, file, pos) = fixture_multi(main_path, main_text, vec![(foo_path, foo_text)]);

    let loc = goto_definition(&db, file, pos).expect("expected definition location");
    assert!(
        loc.uri.as_str().contains("Foo.java"),
        "expected goto-definition to resolve to Foo.java; got {:?}",
        loc.uri
    );
    assert_eq!(loc.range.start.line, 2);
}

#[test]
fn goto_definition_resolves_method_reference_across_files() {
    let main_path = PathBuf::from("/workspace/src/main/java/Main.java");
    let foo_path = PathBuf::from("/workspace/src/main/java/Foo.java");

    let (foo_text, foo_offset) = strip_marker("class Foo { void $1bar(){} }", "$1");
    let (main_text, main_offset) = strip_marker(
        "import java.util.function.Consumer; class Main { void m(){ Consumer<Foo> c = Foo::$0bar; } }",
        "$0",
    );

    let mut db = InMemoryFileStore::new();
    let main_file = db.file_id_for_path(&main_path);
    let foo_file = db.file_id_for_path(&foo_path);
    db.set_file_text(main_file, main_text.clone());
    db.set_file_text(foo_file, foo_text.clone());

    let pos = offset_to_position(&main_text, main_offset);
    let loc = goto_definition(&db, main_file, pos).expect("expected definition location");

    assert!(
        loc.uri.as_str().contains("Foo.java"),
        "expected goto-definition to resolve to Foo.java; got {:?}",
        loc.uri
    );

    assert_eq!(
        loc.range.start,
        offset_to_position(&foo_text, foo_offset),
        "expected goto-definition to resolve to Foo.bar() definition"
    );
}

#[test]
fn goto_definition_resolves_method_reference_with_generic_receiver_across_files() {
    let main_path = PathBuf::from("/workspace/src/main/java/Main.java");
    let foo_path = PathBuf::from("/workspace/src/main/java/Foo.java");

    let (foo_text, foo_offset) = strip_marker("class Foo<T> { void $1bar(){} }", "$1");
    let (main_text, main_offset) = strip_marker(
        "import java.util.function.Consumer; class Main { void m(){ Consumer<Foo<String>> c = Foo<String>::$0bar; } }",
        "$0",
    );

    let mut db = InMemoryFileStore::new();
    let main_file = db.file_id_for_path(&main_path);
    let foo_file = db.file_id_for_path(&foo_path);
    db.set_file_text(main_file, main_text.clone());
    db.set_file_text(foo_file, foo_text.clone());

    let pos = offset_to_position(&main_text, main_offset);
    let loc = goto_definition(&db, main_file, pos).expect("expected definition location");

    assert!(
        loc.uri.as_str().contains("Foo.java"),
        "expected goto-definition to resolve to Foo.java; got {:?}",
        loc.uri
    );
    assert_eq!(
        loc.range.start,
        offset_to_position(&foo_text, foo_offset),
        "expected goto-definition to resolve to Foo.bar() definition"
    );
}

#[test]
fn goto_definition_resolves_method_reference_with_call_receiver_across_files() {
    let main_path = PathBuf::from("/workspace/src/main/java/Main.java");
    let foo_path = PathBuf::from("/workspace/src/main/java/Foo.java");

    let (foo_text, foo_offset) = strip_marker("class Foo { void $1bar(){} }", "$1");
    let (main_text, main_offset) = strip_marker(
        "import java.util.function.Consumer; class Main { Foo foo(){ return new Foo(); } void m(){ Consumer<Foo> c = foo()::$0bar; } }",
        "$0",
    );

    let mut db = InMemoryFileStore::new();
    let main_file = db.file_id_for_path(&main_path);
    let foo_file = db.file_id_for_path(&foo_path);
    db.set_file_text(main_file, main_text.clone());
    db.set_file_text(foo_file, foo_text.clone());

    let pos = offset_to_position(&main_text, main_offset);
    let loc = goto_definition(&db, main_file, pos).expect("expected definition location");

    assert!(
        loc.uri.as_str().contains("Foo.java"),
        "expected goto-definition to resolve to Foo.java; got {:?}",
        loc.uri
    );
    assert_eq!(
        loc.range.start,
        offset_to_position(&foo_text, foo_offset),
        "expected goto-definition to resolve to Foo.bar() definition"
    );
}

#[test]
fn goto_definition_resolves_method_reference_with_new_expression_receiver_across_files() {
    let main_path = PathBuf::from("/workspace/src/main/java/Main.java");
    let foo_path = PathBuf::from("/workspace/src/main/java/Foo.java");

    let (foo_text, foo_offset) = strip_marker("class Foo { void $1bar(){} }", "$1");
    let (main_text, main_offset) = strip_marker(
        "import java.util.function.Consumer; class Main { void m(){ Consumer<Foo> c = new Foo()::$0bar; } }",
        "$0",
    );

    let mut db = InMemoryFileStore::new();
    let main_file = db.file_id_for_path(&main_path);
    let foo_file = db.file_id_for_path(&foo_path);
    db.set_file_text(main_file, main_text.clone());
    db.set_file_text(foo_file, foo_text.clone());

    let pos = offset_to_position(&main_text, main_offset);
    let loc = goto_definition(&db, main_file, pos).expect("expected definition location");

    assert!(
        loc.uri.as_str().contains("Foo.java"),
        "expected goto-definition to resolve to Foo.java; got {:?}",
        loc.uri
    );
    assert_eq!(
        loc.range.start,
        offset_to_position(&foo_text, foo_offset),
        "expected goto-definition to resolve to Foo.bar() definition"
    );
}

#[test]
fn goto_definition_resolves_constructor_reference_to_type_across_files() {
    let main_path = PathBuf::from("/workspace/src/main/java/Main.java");
    let foo_path = PathBuf::from("/workspace/src/main/java/Foo.java");

    let (foo_text, foo_offset) = strip_marker("class $1Foo {}", "$1");
    let (main_text, main_offset) = strip_marker(
        "import java.util.function.Supplier; class Main { void m(){ Supplier<Foo> s = $0Foo::new; } }",
        "$0",
    );

    let mut db = InMemoryFileStore::new();
    let main_file = db.file_id_for_path(&main_path);
    let foo_file = db.file_id_for_path(&foo_path);
    db.set_file_text(main_file, main_text.clone());
    db.set_file_text(foo_file, foo_text.clone());

    let pos = offset_to_position(&main_text, main_offset);
    let loc = goto_definition(&db, main_file, pos).expect("expected definition location");

    assert!(
        loc.uri.as_str().contains("Foo.java"),
        "expected goto-definition to resolve to Foo.java; got {:?}",
        loc.uri
    );
    assert_eq!(
        loc.range.start,
        offset_to_position(&foo_text, foo_offset),
        "expected goto-definition to resolve to Foo type definition"
    );
}

#[test]
fn goto_definition_resolves_constructor_reference_new_token_to_type_across_files() {
    let main_path = PathBuf::from("/workspace/src/main/java/Main.java");
    let foo_path = PathBuf::from("/workspace/src/main/java/Foo.java");

    let (foo_text, foo_offset) = strip_marker("class $1Foo {}", "$1");
    let (main_text, main_offset) = strip_marker(
        "import java.util.function.Supplier; class Main { void m(){ Supplier<Foo> s = Foo::$0new; } }",
        "$0",
    );

    let mut db = InMemoryFileStore::new();
    let main_file = db.file_id_for_path(&main_path);
    let foo_file = db.file_id_for_path(&foo_path);
    db.set_file_text(main_file, main_text.clone());
    db.set_file_text(foo_file, foo_text.clone());

    let pos = offset_to_position(&main_text, main_offset);
    let loc = goto_definition(&db, main_file, pos).expect("expected definition location");

    assert!(
        loc.uri.as_str().contains("Foo.java"),
        "expected goto-definition to resolve to Foo.java; got {:?}",
        loc.uri
    );
    assert_eq!(
        loc.range.start,
        offset_to_position(&foo_text, foo_offset),
        "expected goto-definition to resolve to Foo type definition"
    );
}

#[test]
fn goto_definition_resolves_constructor_reference_with_generic_receiver_across_files() {
    let main_path = PathBuf::from("/workspace/src/main/java/Main.java");
    let foo_path = PathBuf::from("/workspace/src/main/java/Foo.java");

    let (foo_text, foo_offset) = strip_marker("class $1Foo<T> {}", "$1");
    let (main_text, main_offset) = strip_marker(
        "import java.util.function.Supplier; class Main { void m(){ Supplier<Foo<String>> s = Foo<String>::$0new; } }",
        "$0",
    );

    let mut db = InMemoryFileStore::new();
    let main_file = db.file_id_for_path(&main_path);
    let foo_file = db.file_id_for_path(&foo_path);
    db.set_file_text(main_file, main_text.clone());
    db.set_file_text(foo_file, foo_text.clone());

    let pos = offset_to_position(&main_text, main_offset);
    let loc = goto_definition(&db, main_file, pos).expect("expected definition location");

    assert!(
        loc.uri.as_str().contains("Foo.java"),
        "expected goto-definition to resolve to Foo.java; got {:?}",
        loc.uri
    );
    assert_eq!(
        loc.range.start,
        offset_to_position(&foo_text, foo_offset),
        "expected goto-definition to resolve to Foo type definition"
    );
}

#[test]
fn goto_definition_resolves_array_constructor_reference_receiver_to_element_type_across_files() {
    let main_path = PathBuf::from("/workspace/src/main/java/Main.java");
    let foo_path = PathBuf::from("/workspace/src/main/java/Foo.java");

    let (foo_text, foo_offset) = strip_marker("class $1Foo {}", "$1");
    let (main_text, main_offset) = strip_marker(
        "import java.util.function.IntFunction; class Main { void m(){ IntFunction<Foo[]> f = Foo[]::$0new; } }",
        "$0",
    );

    let mut db = InMemoryFileStore::new();
    let main_file = db.file_id_for_path(&main_path);
    let foo_file = db.file_id_for_path(&foo_path);
    db.set_file_text(main_file, main_text.clone());
    db.set_file_text(foo_file, foo_text.clone());

    let pos = offset_to_position(&main_text, main_offset);
    let loc = goto_definition(&db, main_file, pos).expect("expected definition location");

    assert!(
        loc.uri.as_str().contains("Foo.java"),
        "expected goto-definition to resolve to Foo.java; got {:?}",
        loc.uri
    );
    assert_eq!(
        loc.range.start,
        offset_to_position(&foo_text, foo_offset),
        "expected goto-definition to resolve to Foo type definition"
    );
}

#[test]
fn find_references_resolves_method_across_files() {
    let foo_path = PathBuf::from("/workspace/src/main/java/Foo.java");
    let main_path = PathBuf::from("/workspace/src/main/java/Main.java");

    let foo_text = r#"
class Foo {
  void ba<|>r() {}
}
"#;
    let main_text = r#"
class Main {
  void m() {
    Foo foo = new Foo();
    foo.bar();
  }
}
"#
    .to_string();

    let (db, file, pos) = fixture_multi(foo_path, foo_text, vec![(main_path, main_text)]);

    let refs = find_references(&db, file, pos, true);
    assert!(
        refs.iter().any(|loc| loc.uri.as_str().contains("Foo.java")),
        "expected references to include declaration; got {refs:#?}"
    );
    assert!(
        refs.iter()
            .any(|loc| loc.uri.as_str().contains("Main.java")),
        "expected references to include call site in Main.java; got {refs:#?}"
    );
}

#[test]
fn find_references_includes_method_reference_usages() {
    let foo_path = PathBuf::from("/workspace/src/main/java/Foo.java");
    let main_path = PathBuf::from("/workspace/src/main/java/Main.java");

    let foo_text = r#"
class Foo {
  void ba<|>r() {}
}
"#;
    let main_text = r#"
class Main {
  void m() {
    Foo foo = new Foo();
    Runnable r1 = foo::bar;
    Runnable r2 = Foo::bar;
  }
}
"#
    .to_string();

    let (db, file, pos) = fixture_multi(foo_path, foo_text, vec![(main_path, main_text)]);

    let refs = find_references(&db, file, pos, false);

    assert!(
        !refs.iter().any(|loc| loc.uri.as_str().contains("Foo.java")),
        "expected references to exclude declaration; got {refs:#?}"
    );

    let main_refs: Vec<_> = refs
        .iter()
        .filter(|loc| loc.uri.as_str().contains("Main.java"))
        .collect();
    assert_eq!(
        main_refs.len(),
        2,
        "expected references to include both method references in Main.java; got {refs:#?}"
    );
}

#[test]
fn find_references_resolves_field_across_files() {
    let foo_path = PathBuf::from("/workspace/src/main/java/Foo.java");
    let main_path = PathBuf::from("/workspace/src/main/java/Main.java");

    let foo_text = r#"
class Foo {
  int va<|>l;
}
"#;
    let main_text = r#"
class Main {
  void m() {
    Foo foo = new Foo();
    int x = foo.val;
  }
}
"#
    .to_string();

    let (db, file, pos) = fixture_multi(foo_path, foo_text, vec![(main_path, main_text)]);

    let refs = find_references(&db, file, pos, true);
    assert!(
        refs.iter().any(|loc| loc.uri.as_str().contains("Foo.java")),
        "expected references to include declaration; got {refs:#?}"
    );
    assert!(
        refs.iter()
            .any(|loc| loc.uri.as_str().contains("Main.java")),
        "expected references to include access in Main.java; got {refs:#?}"
    );
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
        diags
            .iter()
            .any(|d| d.severity == Severity::Error
                && d.message.contains("Cannot resolve symbol 'baz'")),
        "expected unresolved symbol diagnostic; got {diags:#?}"
    );
}

#[test]
fn diagnostics_include_unresolved_import() {
    let (db, file) = fixture_file(
        r#"
import does.not.Exist;
class A {}
"#,
    );

    let diags = file_diagnostics(&db, file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "unresolved-import"),
        "expected unresolved-import diagnostic; got {diags:#?}"
    );
}

#[test]
fn diagnostics_include_language_level_feature_gate() {
    use tempfile::TempDir;

    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path().join("project");
    std::fs::create_dir_all(root.join("src/main/java")).expect("create source root");

    // Configure a Java language level below records (Java 16).
    std::fs::write(
        root.join("pom.xml"),
        r#"<project>
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>example</artifactId>
  <version>0.1.0</version>
  <properties>
    <maven.compiler.source>11</maven.compiler.source>
    <maven.compiler.target>11</maven.compiler.target>
  </properties>
</project>
"#,
    )
    .expect("write pom.xml");

    let file_path = root.join("src/main/java/Main.java");
    std::fs::write(&file_path, "").expect("touch Main.java");

    let mut db = InMemoryFileStore::new();
    let file = db.file_id_for_path(&file_path);
    db.set_file_text(file, "record Point(int x, int y) {}\n".to_string());

    let diags = file_diagnostics(&db, file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "JAVA_FEATURE_RECORDS"),
        "expected JAVA_FEATURE_RECORDS diagnostic; got {diags:#?}"
    );
}

#[test]
fn spring_value_completion_uses_config_keys() {
    let config_path = PathBuf::from("/workspace/src/main/resources/application.properties");
    let java_path = PathBuf::from("/workspace/src/main/java/C.java");

    let config_text = "server.port=8080\n".to_string();
    let java_text = r#"
import org.springframework.beans.factory.annotation.Value;
class C {
  @Value("${ser<|>}")
  String port;
}
"#;

    let (db, file, pos) = fixture_multi(java_path, java_text, vec![(config_path, config_text)]);

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"server.port"),
        "expected Spring config completion; got {labels:?}"
    );
}

#[test]
fn spring_value_completion_replaces_full_placeholder_prefix() {
    let config_path = PathBuf::from("/workspace/src/main/resources/application.properties");
    let java_path = PathBuf::from("/workspace/src/main/java/C.java");

    let config_text = "server.port=8080\n".to_string();
    let java_text = r#"
import org.springframework.beans.factory.annotation.Value;
class C {
  @Value("${server.p<|>}")
  String port;
}
"#;

    let (db, file, pos) = fixture_multi(java_path, java_text, vec![(config_path, config_text)]);

    let java_without_caret = java_text.replace("<|>", "");
    let key_start = java_without_caret
        .find("server.p")
        .expect("expected placeholder prefix in fixture");

    let items = completions(&db, file, pos);
    let item = items
        .iter()
        .find(|i| i.label == "server.port")
        .expect("expected server.port completion item");

    let edit = match item.text_edit.as_ref().expect("expected text_edit") {
        CompletionTextEdit::Edit(edit) => edit,
        other => panic!("unexpected text_edit variant: {other:?}"),
    };

    assert_eq!(
        edit.range.start,
        offset_to_position(&java_without_caret, key_start)
    );
    assert_eq!(edit.range.end, pos);
}

#[test]
fn spring_goto_definition_jumps_to_config_file() {
    let config_path = PathBuf::from("/workspace/src/main/resources/application.properties");
    let java_path = PathBuf::from("/workspace/src/main/java/C.java");

    let config_text = "server.port=8080\n".to_string();
    let java_text = r#"
import org.springframework.beans.factory.annotation.Value;
class C {
  @Value("${server.<|>port}")
  String port;
}
"#;

    let (db, file, pos) = fixture_multi(
        java_path,
        java_text,
        vec![(config_path.clone(), config_text)],
    );

    let loc = goto_definition(&db, file, pos).expect("expected config definition location");
    assert!(
        loc.uri.as_str().contains("application.properties"),
        "expected definition URI to point at config file; got {:?}",
        loc.uri
    );
    assert_eq!(loc.range.start.line, 0);
    assert_eq!(loc.range.start.character, 0);
}

#[test]
fn spring_properties_key_completion_replaces_full_key_prefix() {
    let config_path = PathBuf::from("/workspace/src/main/resources/application.properties");

    let config_text = r#"server.port=8080
server.p<|>
"#;

    let (db, file, pos) = fixture_multi(config_path, config_text, vec![]);

    let config_without_caret = config_text.replace("<|>", "");
    let key_start = config_without_caret
        .rfind("server.p")
        .expect("expected second server.p key in fixture");

    let items = completions(&db, file, pos);
    let item = items
        .iter()
        .find(|i| i.label == "server.port")
        .expect("expected server.port completion item");

    let edit = match item.text_edit.as_ref().expect("expected text_edit") {
        CompletionTextEdit::Edit(edit) => edit,
        other => panic!("unexpected text_edit variant: {other:?}"),
    };

    assert_eq!(
        edit.range.start,
        offset_to_position(&config_without_caret, key_start)
    );
    assert_eq!(edit.range.end, pos);
}

#[test]
fn spring_yaml_key_completion_replaces_full_segment_prefix() {
    let config_path = PathBuf::from("/workspace/src/main/resources/application.yml");

    let config_text = r#"spring:
  main:
    banner-mode: console
    banner-m<|>
"#;

    let (db, file, pos) = fixture_multi(config_path, config_text, vec![]);

    let config_without_caret = config_text.replace("<|>", "");
    let key_start = config_without_caret
        .rfind("banner-m")
        .expect("expected banner-m key in fixture");

    let items = completions(&db, file, pos);
    let item = items
        .iter()
        .find(|i| i.label == "banner-mode")
        .expect("expected banner-mode completion item");

    let edit = match item.text_edit.as_ref().expect("expected text_edit") {
        CompletionTextEdit::Edit(edit) => edit,
        other => panic!("unexpected text_edit variant: {other:?}"),
    };

    assert_eq!(
        edit.range.start,
        offset_to_position(&config_without_caret, key_start)
    );
    assert_eq!(edit.range.end, pos);
}

#[test]
fn spring_find_references_from_config_key_to_java_usage() {
    let config_path = PathBuf::from("/workspace/src/main/resources/application.properties");
    let java_path = PathBuf::from("/workspace/src/main/java/C.java");

    let config_text = "server.<|>port=8080\n";
    let java_text = r#"
import org.springframework.beans.factory.annotation.Value;
class C {
  @Value("${server.port}")
  String port;
}
"#;

    let (db, config_file, pos) = fixture_multi(
        config_path.clone(),
        config_text,
        vec![(java_path.clone(), java_text.to_string())],
    );

    let refs = find_references(&db, config_file, pos, false);
    assert_eq!(refs.len(), 1);
    assert!(
        refs[0].uri.as_str().contains("C.java"),
        "expected reference to point at Java file; got {:?}",
        refs[0].uri
    );
}

#[test]
fn spring_find_references_from_value_placeholder_to_config_key() {
    let config_path = PathBuf::from("/workspace/src/main/resources/application.properties");
    let java_path = PathBuf::from("/workspace/src/main/java/C.java");

    let config_text = "server.port=8080\n".to_string();
    let java_text = r#"
import org.springframework.beans.factory.annotation.Value;
class C {
  @Value("${server.<|>port}")
  String port;
}
"#;

    let (db, java_file, pos) = fixture_multi(
        java_path.clone(),
        java_text,
        vec![(config_path.clone(), config_text)],
    );

    let refs = find_references(&db, java_file, pos, true);
    assert_eq!(refs.len(), 2, "expected 2 references; got {refs:#?}");
    assert!(
        refs.iter()
            .any(|loc| loc.uri.as_str().contains("application.properties")),
        "expected references to include config definition; got {refs:#?}"
    );
    assert!(
        refs.iter().any(|loc| loc.uri.as_str().contains("C.java")),
        "expected references to include Java usage; got {refs:#?}"
    );
}

#[test]
fn spring_config_diagnostics_include_duplicate_keys() {
    let config_path = PathBuf::from("/workspace/src/main/resources/application.properties");
    let mut db = InMemoryFileStore::new();
    let file = db.file_id_for_path(&config_path);
    db.set_file_text(file, "server.port=8080\nserver.port=9090\n".to_string());

    let diags = file_diagnostics(&db, file);
    assert!(
        diags
            .iter()
            .any(|d| d.message.contains("Duplicate configuration key")),
        "expected duplicate key diagnostic; got {diags:#?}"
    );
}

#[test]
fn document_symbols_include_class_with_method_child() {
    let (db, file) = fixture_file(
        r#"
class A {
  void foo() {}
}
"#,
    );

    let symbols = document_symbols(&db, file);
    assert_eq!(
        symbols.len(),
        1,
        "expected one class symbol; got {symbols:#?}"
    );

    let class = &symbols[0];
    assert_eq!(class.name, "A");
    assert_eq!(class.kind, lsp_types::SymbolKind::CLASS);

    let children = class
        .children
        .as_ref()
        .expect("expected class symbol to have children");

    assert!(
        children
            .iter()
            .any(|sym| sym.name == "foo" && sym.kind == lsp_types::SymbolKind::METHOD),
        "expected foo method child; got {children:#?}"
    );
}

#[test]
fn call_hierarchy_outgoing_calls_include_call_site_ranges() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void bar() {}
  void foo() {
    <|>
    bar();
  }
}
"#,
    );

    let items =
        prepare_call_hierarchy(&db, file, pos).expect("expected call hierarchy preparation");
    assert_eq!(items.len(), 1);

    let outgoing = call_hierarchy_outgoing_calls(&db, file, &items[0].name);
    assert_eq!(
        outgoing.len(),
        1,
        "expected one outgoing call; got {outgoing:#?}"
    );
    assert_eq!(outgoing[0].to.name, "bar");
    assert!(
        !outgoing[0].from_ranges.is_empty(),
        "expected outgoing call to have from_ranges"
    );
    assert!(
        outgoing[0].from_ranges[0].start != outgoing[0].from_ranges[0].end,
        "expected non-empty call-site range; got {:?}",
        outgoing[0].from_ranges[0]
    );
}

#[test]
fn type_hierarchy_prepare_and_supertypes_subtypes_work() {
    let (db, file, pos) = fixture(
        r#"
class A {}
class B<|> extends A {}
"#,
    );

    let items =
        prepare_type_hierarchy(&db, file, pos).expect("expected type hierarchy preparation");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].name, "B");

    let supers = type_hierarchy_supertypes(&db, file, &items[0].name);
    assert_eq!(supers.len(), 1);
    assert_eq!(supers[0].name, "A");

    let subs = type_hierarchy_subtypes(&db, file, "A");
    assert_eq!(subs.len(), 1);
    assert_eq!(subs[0].name, "B");
}

#[test]
fn signature_help_resolves_jdk_method() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    "x".substring(<|>0, 1);
  }
}
"#,
    );

    let sig = signature_help(&db, file, pos).expect("expected signature help");
    let labels: Vec<_> = sig.signatures.iter().map(|s| s.label.as_str()).collect();
    assert!(
        labels.iter().any(|l| l.contains("String substring(int")),
        "expected signature help to mention `String substring(int`; got {labels:?}"
    );
}

#[test]
fn hover_shows_inferred_type_for_var_literal() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    var x = "hello";
    x<|>.toString();
  }
}
"#,
    );

    let hover = hover(&db, file, pos).expect("expected hover");
    let value = match hover.contents {
        lsp_types::HoverContents::Markup(markup) => markup.value,
        other => format!("{other:?}"),
    };
    assert!(
        value.contains("x: String"),
        "expected hover to contain inferred type `String`; got {value:?}"
    );
}

#[test]
fn inlay_hints_include_parameter_names_for_jdk_call() {
    let (db, file) = fixture_file(
        r#"
class A {
  void m() {
    "x".substring(0, 1);
  }
}
"#,
    );

    let range = lsp_types::Range::new(
        lsp_types::Position::new(0, 0),
        lsp_types::Position::new(999, 999),
    );
    let hints = inlay_hints(&db, file, range);
    let param_labels: Vec<String> = hints
        .into_iter()
        .filter(|h| h.kind == Some(lsp_types::InlayHintKind::PARAMETER))
        .filter_map(|h| match h.label {
            lsp_types::InlayHintLabel::String(s) => Some(s),
            _ => None,
        })
        .collect();

    assert!(
        param_labels.iter().any(|l| l.contains("beginIndex:")),
        "expected parameter inlay hint for beginIndex; got {param_labels:?}"
    );
    assert!(
        param_labels.iter().any(|l| l.contains("endIndex:")),
        "expected parameter inlay hint for endIndex; got {param_labels:?}"
    );
}

#[test]
fn diagnostics_include_unreachable_code() {
    let (db, file) = fixture_file(
        r#"
class A {
  void m() {
    return;
    int x = 1;
  }
}
"#,
    );

    let diags = file_diagnostics(&db, file);
    assert!(
        diags
            .iter()
            .any(|d| d.code == "FLOW_UNREACHABLE" && d.severity == Severity::Warning),
        "expected unreachable-code diagnostic; got {diags:#?}"
    );
}

#[test]
fn diagnostics_include_use_before_assignment() {
    let (db, file) = fixture_file(
        r#"
class A {
  void m() {
    int x;
    int y = x;
  }
}
"#,
    );

    let diags = file_diagnostics(&db, file);
    assert!(
        diags
            .iter()
            .any(|d| d.code == "FLOW_UNASSIGNED" && d.severity == Severity::Error),
        "expected use-before-assignment diagnostic; got {diags:#?}"
    );
}

#[test]
fn diagnostics_include_possible_null_dereference() {
    let (db, file) = fixture_file(
        r#"
class A {
  void m() {
    String s = null;
    s.length();
  }
}
"#,
    );

    let diags = file_diagnostics(&db, file);
    assert!(
        diags
            .iter()
            .any(|d| d.code == "FLOW_NULL_DEREF" && d.severity == Severity::Warning),
        "expected null-dereference diagnostic; got {diags:#?}"
    );
}

#[test]
fn diagnostics_include_unreachable_code_in_constructor() {
    let (db, file) = fixture_file(
        r#"
class A {
  A() {
    return;
    int x = 1;
  }
}
"#,
    );

    let diags = file_diagnostics(&db, file);
    assert!(
        diags
            .iter()
            .any(|d| d.code == "FLOW_UNREACHABLE" && d.severity == Severity::Warning),
        "expected unreachable-code diagnostic in constructor; got {diags:#?}"
    );
}

#[test]
fn diagnostics_include_unreachable_code_in_initializer() {
    let (db, file) = fixture_file(
        r#"
class A {
  {
    throw new RuntimeException();
    int x = 1;
  }
}
"#,
    );

    let diags = file_diagnostics(&db, file);
    assert!(
        diags
            .iter()
            .any(|d| d.code == "FLOW_UNREACHABLE" && d.severity == Severity::Warning),
        "expected unreachable-code diagnostic in initializer; got {diags:#?}"
    );
}

#[test]
fn diagnostics_do_not_mark_finally_unreachable_on_return() {
    let text = r#"
class A {
  void m() {
    try {
      return;
    } finally {
      int x = 1;
    }
    int y = 2;
  }
}
"#;

    let (db, file) = fixture_file(text);
    let diags = file_diagnostics(&db, file);

    let x_needle = "int x = 1;";
    let x_start = text.find(x_needle).expect("expected int x in fixture");
    let x_end = x_start + x_needle.len();

    let y_needle = "int y = 2;";
    let y_start = text.find(y_needle).expect("expected int y in fixture");
    let y_end = y_start + y_needle.len();

    assert!(
        !diags.iter().any(|d| d.code == "FLOW_UNREACHABLE"
            && d.span
                .is_some_and(|span| span.start < x_end && span.end > x_start)),
        "expected finally block to be reachable; got {diags:#?}"
    );
    assert!(
        diags.iter().any(|d| d.code == "FLOW_UNREACHABLE"
            && d.span
                .is_some_and(|span| span.start < y_end && span.end > y_start)),
        "expected statement after try/finally to be unreachable; got {diags:#?}"
    );
}

#[test]
fn diagnostics_do_not_mark_outer_finally_unreachable_on_nested_return() {
    let text = r#"
class A {
  void m() {
    try {
      try {
        return;
      } finally {
        int x = 1;
      }
    } finally {
      int y = 2;
    }
  }
}
"#;

    let (db, file) = fixture_file(text);
    let diags = file_diagnostics(&db, file);

    let x_needle = "int x = 1;";
    let x_start = text.find(x_needle).expect("expected int x in fixture");
    let x_end = x_start + x_needle.len();

    let y_needle = "int y = 2;";
    let y_start = text.find(y_needle).expect("expected int y in fixture");
    let y_end = y_start + y_needle.len();

    assert!(
        !diags.iter().any(|d| d.code == "FLOW_UNREACHABLE"
            && d.span
                .is_some_and(|span| span.start < x_end && span.end > x_start)),
        "expected inner finally block to be reachable; got {diags:#?}"
    );
    assert!(
        !diags.iter().any(|d| d.code == "FLOW_UNREACHABLE"
            && d.span
                .is_some_and(|span| span.start < y_end && span.end > y_start)),
        "expected outer finally block to be reachable; got {diags:#?}"
    );
}

#[test]
fn diagnostics_report_use_before_assignment_after_break_inside_try() {
    let text = r#"
class A {
  void m() {
    int x;
    try {
      for (;;) {
        break;
      }
      int y = x;
    } finally {
      x = 1;
    }
  }
}
"#;

    let (db, file) = fixture_file(text);
    let diags = file_diagnostics(&db, file);

    let needle = "int y = x;";
    let start = text
        .find(needle)
        .unwrap_or_else(|| panic!("expected `{needle}` in fixture"));
    let end = start + needle.len();

    assert!(
        diags.iter().any(|d| d.code == "FLOW_UNASSIGNED"
            && d.severity == Severity::Error
            && d.span
                .is_some_and(|span| span.start < end && span.end > start)),
        "expected use-before-assignment diagnostic after break; got {diags:#?}"
    );
}

#[test]
fn diagnostics_report_unreachable_else_branch_on_if_true() {
    let text = r#"
class A {
  void m() {
    if (true) {
      int x = 1;
    } else {
      int y = 2;
    }
  }
}
"#;

    let (db, file) = fixture_file(text);
    let diags = file_diagnostics(&db, file);

    let x_needle = "int x = 1;";
    let x_start = text.find(x_needle).expect("expected int x in fixture");
    let x_end = x_start + x_needle.len();

    let y_needle = "int y = 2;";
    let y_start = text.find(y_needle).expect("expected int y in fixture");
    let y_end = y_start + y_needle.len();

    assert!(
        !diags.iter().any(|d| d.code == "FLOW_UNREACHABLE"
            && d.span
                .is_some_and(|span| span.start < x_end && span.end > x_start)),
        "expected then branch to be reachable; got {diags:#?}"
    );
    assert!(
        diags.iter().any(|d| d.code == "FLOW_UNREACHABLE"
            && d.span
                .is_some_and(|span| span.start < y_end && span.end > y_start)),
        "expected else branch to be unreachable; got {diags:#?}"
    );
}

#[test]
fn diagnostics_report_unreachable_while_body_on_constant_false() {
    let text = r#"
class A {
  void m() {
    while (false) {
      int x = 1;
    }
    int y = 2;
  }
}
"#;

    let (db, file) = fixture_file(text);
    let diags = file_diagnostics(&db, file);

    let x_needle = "int x = 1;";
    let x_start = text.find(x_needle).expect("expected int x in fixture");
    let x_end = x_start + x_needle.len();

    let y_needle = "int y = 2;";
    let y_start = text.find(y_needle).expect("expected int y in fixture");
    let y_end = y_start + y_needle.len();

    assert!(
        diags.iter().any(|d| d.code == "FLOW_UNREACHABLE"
            && d.span
                .is_some_and(|span| span.start < x_end && span.end > x_start)),
        "expected while body to be unreachable; got {diags:#?}"
    );
    assert!(
        !diags.iter().any(|d| d.code == "FLOW_UNREACHABLE"
            && d.span
                .is_some_and(|span| span.start < y_end && span.end > y_start)),
        "expected statement after while(false) to be reachable; got {diags:#?}"
    );
}

#[test]
fn diagnostics_report_unreachable_after_for_true() {
    let text = r#"
class A {
  void m() {
    for (; true;) {
      int x = 1;
    }
    int y = 2;
  }
}
"#;

    let (db, file) = fixture_file(text);
    let diags = file_diagnostics(&db, file);

    let x_needle = "int x = 1;";
    let x_start = text.find(x_needle).expect("expected int x in fixture");
    let x_end = x_start + x_needle.len();

    let y_needle = "int y = 2;";
    let y_start = text.find(y_needle).expect("expected int y in fixture");
    let y_end = y_start + y_needle.len();

    assert!(
        !diags.iter().any(|d| d.code == "FLOW_UNREACHABLE"
            && d.span
                .is_some_and(|span| span.start < x_end && span.end > x_start)),
        "expected for-body to be reachable; got {diags:#?}"
    );
    assert!(
        diags.iter().any(|d| d.code == "FLOW_UNREACHABLE"
            && d.span
                .is_some_and(|span| span.start < y_end && span.end > y_start)),
        "expected statement after for(;true;) to be unreachable; got {diags:#?}"
    );
}

#[test]
fn diagnostics_do_not_warn_null_deref_after_and_and_null_check() {
    let text = r#"
class A {
  void m(String s, boolean cond) {
    if (s != null && cond) {
      s.length();
    }
  }
}
"#;

    let (db, file) = fixture_file(text);
    let diags = file_diagnostics(&db, file);

    let needle = "s.length();";
    let start = text.find(needle).expect("expected s.length in fixture");
    let end = start + needle.len();

    assert!(
        !diags.iter().any(|d| d.code == "FLOW_NULL_DEREF"
            && d.span
                .is_some_and(|span| span.start < end && span.end > start)),
        "expected no null-dereference diagnostic after null check; got {diags:#?}"
    );
}

#[test]
fn diagnostics_do_not_warn_null_deref_after_or_or_null_check() {
    let text = r#"
class A {
  void m(String s, boolean cond) {
    if (s == null || cond) {
      return;
    }
    s.length();
  }
}
"#;

    let (db, file) = fixture_file(text);
    let diags = file_diagnostics(&db, file);

    let needle = "s.length();";
    let start = text.find(needle).expect("expected s.length in fixture");
    let end = start + needle.len();

    assert!(
        !diags.iter().any(|d| d.code == "FLOW_NULL_DEREF"
            && d.span
                .is_some_and(|span| span.start < end && span.end > start)),
        "expected no null-dereference diagnostic after null check; got {diags:#?}"
    );
}

#[test]
fn diagnostics_do_not_warn_null_deref_in_and_and_condition_guard() {
    let text = r#"
class A {
  void m(String s) {
    if (s != null && s.length() == 0) {
      return;
    }
  }
}
"#;

    let (db, file) = fixture_file(text);
    let diags = file_diagnostics(&db, file);

    let needle = "s.length()";
    let start = text.find(needle).expect("expected s.length in fixture");
    let end = start + needle.len();

    assert!(
        !diags.iter().any(|d| d.code == "FLOW_NULL_DEREF"
            && d.span
                .is_some_and(|span| span.start < end && span.end > start)),
        "expected no null-dereference diagnostic for short-circuited condition; got {diags:#?}"
    );
}

#[test]
fn diagnostics_do_not_warn_null_deref_in_or_or_condition_guard() {
    let text = r#"
class A {
  void m(String s) {
    if (s == null || s.length() == 0) {
      return;
    }
  }
}
"#;

    let (db, file) = fixture_file(text);
    let diags = file_diagnostics(&db, file);

    let needle = "s.length()";
    let start = text.find(needle).expect("expected s.length in fixture");
    let end = start + needle.len();

    assert!(
        !diags.iter().any(|d| d.code == "FLOW_NULL_DEREF"
            && d.span
                .is_some_and(|span| span.start < end && span.end > start)),
        "expected no null-dereference diagnostic for short-circuited condition; got {diags:#?}"
    );
}

#[test]
fn diagnostics_do_not_report_use_before_assignment_in_false_and_and_condition() {
    let text = r#"
class A {
  void m() {
    int x;
    if (false && x == 0) {
      return;
    }
  }
}
"#;

    let (db, file) = fixture_file(text);
    let diags = file_diagnostics(&db, file);
    assert!(
        !diags.iter().any(|d| d.code == "FLOW_UNASSIGNED"),
        "expected no use-before-assignment diagnostic for short-circuited rhs; got {diags:#?}"
    );
}

#[test]
fn diagnostics_do_not_report_use_before_assignment_in_true_or_or_condition() {
    let text = r#"
class A {
  void m() {
    int x;
    if (true || x == 0) {
      return;
    }
  }
}
"#;

    let (db, file) = fixture_file(text);
    let diags = file_diagnostics(&db, file);
    assert!(
        !diags.iter().any(|d| d.code == "FLOW_UNASSIGNED"),
        "expected no use-before-assignment diagnostic for short-circuited rhs; got {diags:#?}"
    );
}

#[test]
fn diagnostics_do_not_warn_null_deref_when_and_and_lhs_is_known_false() {
    let text = r#"
class A {
  void m() {
    String s = new String();
    if (s == null && s.length() == 0) {
      return;
    }
  }
}
"#;

    let (db, file) = fixture_file(text);
    let diags = file_diagnostics(&db, file);

    let needle = "s.length()";
    let start = text.find(needle).expect("expected s.length in fixture");
    let end = start + needle.len();

    assert!(
        !diags.iter().any(|d| d.code == "FLOW_NULL_DEREF"
            && d.span
                .is_some_and(|span| span.start < end && span.end > start)),
        "expected no null-dereference diagnostic when rhs is unreachable; got {diags:#?}"
    );
}

#[test]
fn diagnostics_do_not_warn_null_deref_when_or_or_lhs_is_known_true() {
    let text = r#"
class A {
  void m() {
    String s = new String();
    if (s != null || s.length() == 0) {
      return;
    }
  }
}
"#;

    let (db, file) = fixture_file(text);
    let diags = file_diagnostics(&db, file);

    let needle = "s.length()";
    let start = text.find(needle).expect("expected s.length in fixture");
    let end = start + needle.len();

    assert!(
        !diags.iter().any(|d| d.code == "FLOW_NULL_DEREF"
            && d.span
                .is_some_and(|span| span.start < end && span.end > start)),
        "expected no null-dereference diagnostic when rhs is unreachable; got {diags:#?}"
    );
}

#[test]
fn completion_scope_excludes_locals_declared_after_cursor() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    int before = 1;
    <|>
    int after = 2;
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"before"),
        "expected `before` to be in completion list; got {labels:?}"
    );
    assert!(
        !labels.contains(&"after"),
        "expected `after` (declared after cursor) to be excluded; got {labels:?}"
    );
}

#[test]
fn completion_scope_excludes_locals_from_ended_blocks() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    if (true) {
      int inner = 1;
    }
    <|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        !labels.contains(&"inner"),
        "expected block-local `inner` to be out of scope; got {labels:?}"
    );
}

#[test]
fn completion_scope_excludes_for_loop_variable_after_loop() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    for (int i = 0; i < 10; i++) {
    }
    <|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        !labels.contains(&"i"),
        "expected `i` (for-loop variable) to be out of scope after loop; got {labels:?}"
    );
}

#[test]
fn completion_scope_excludes_for_loop_variable_after_unbraced_loop() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    for (int i = 0; i < 10; i++)
      System.out.println(i);
    <|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        !labels.contains(&"i"),
        "expected `i` (for-loop variable) to be out of scope after unbraced loop; got {labels:?}"
    );
}

#[test]
fn completion_scope_includes_for_each_variable_inside_loop() {
    let (db, file, pos) = fixture(
        r#"
import java.util.List;
class A {
  void m(List<String> xs) {
    for (String item : xs) {
      <|>
    }
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"item"),
        "expected enhanced-for variable `item` to be in scope inside loop; got {labels:?}"
    );
}

#[test]
fn completion_scope_excludes_for_each_variable_after_loop() {
    let (db, file, pos) = fixture(
        r#"
import java.util.List;
class A {
  void m(List<String> xs) {
    for (String item : xs) {
    }
    <|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        !labels.contains(&"item"),
        "expected enhanced-for variable `item` to be out of scope after loop; got {labels:?}"
    );
}

#[test]
fn completion_scope_excludes_try_resource_after_try() {
    let (db, file, pos) = fixture(
        r#"
import java.io.InputStream;
class A {
  void m() {
    try (InputStream in = null) {
    }
    <|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        !labels.contains(&"in"),
        "expected try-with-resources variable `in` to be out of scope after try; got {labels:?}"
    );
}

#[test]
fn completion_scope_excludes_catch_parameter_after_catch() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    try {
    } catch (RuntimeException e) {
    }
    <|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        !labels.contains(&"e"),
        "expected catch parameter `e` to be out of scope after catch; got {labels:?}"
    );
}

#[test]
fn completion_scope_includes_catch_parameter_inside_catch() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    try {
    } catch (RuntimeException e) {
      <|>
    }
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"e"),
        "expected catch parameter `e` to be in scope inside catch; got {labels:?}"
    );
}

#[test]
fn completion_recency_ranks_recent_locals_first() {
    let (db, file, pos) = fixture(
        r#"
class A {
  void m() {
    int foo = 1;
    int bar = 2;
    System.out.println(foo);
    System.out.println(bar);
    <|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let foo_idx = items
        .iter()
        .position(|i| i.label == "foo")
        .expect("expected `foo` completion");
    let bar_idx = items
        .iter()
        .position(|i| i.label == "bar")
        .expect("expected `bar` completion");
    assert!(
        bar_idx < foo_idx,
        "expected `bar` to rank above `foo` due to recency; got indices bar={bar_idx} foo={foo_idx}"
    );
}

#[test]
fn file_diagnostics_includes_unresolved_import() {
    let mut db = InMemoryFileStore::new();
    let path = PathBuf::from("/test.java");
    let file = db.file_id_for_path(&path);
    db.set_file_text(
        file,
        r#"
import foo.Bar;
class A {}
"#
        .to_string(),
    );

    let diags = file_diagnostics(&db, file);
    assert!(
        diags.iter().any(|d| {
            d.code == "unresolved-import"
                && d.severity == Severity::Error
                && d.message.contains("foo.Bar")
        }),
        "expected unresolved-import diagnostic; got {diags:#?}"
    );
}
