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
