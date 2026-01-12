use lsp_types::{CompletionTextEdit, InsertTextFormat};
use nova_db::InMemoryFileStore;
use nova_ide::{
    call_hierarchy_outgoing_calls, completions, document_symbols, file_diagnostics,
    find_references, goto_definition, hover, inlay_hints, prepare_call_hierarchy,
    prepare_type_hierarchy, signature_help, type_hierarchy_subtypes, type_hierarchy_supertypes,
};
use nova_types::Severity;
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
    let caret = "<|>";
    let caret_offset = primary_text_with_caret
        .find(caret)
        .expect("fixture must contain <|> caret marker");
    let primary_text = primary_text_with_caret.replace(caret, "");
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
fn completion_deduplicates_items_by_label_and_kind() {
    // `Stream` member completions come from two sources:
    // - hardcoded `STREAM_MEMBER_METHODS`
    // - workspace type extraction via the Lombok completion provider
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

    let (db, file, pos) =
        fixture_multi(file_b_path, file_b_text, vec![(file_a_path, file_a_text)]);

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

    assert_eq!(
        edit.range.start,
        offset_to_position(&main_without_caret, my_start)
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
fn completion_does_not_offer_postfix_for_qualified_receiver() {
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

    let items = completions(&db, file, pos);
    assert!(
        !items
            .iter()
            .any(|i| { i.label == "nn" && i.kind == Some(lsp_types::CompletionItemKind::SNIPPET) }),
        "expected no postfix `nn` snippet for qualified receiver; got {items:#?}"
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
fn completion_inside_block_comment_is_empty() {
    let (db, file, pos) = fixture("class A { /* ret<|> */ }");
    let items = completions(&db, file, pos);
    assert!(
        items.is_empty(),
        "expected no completions inside block comment; got {items:#?}"
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
