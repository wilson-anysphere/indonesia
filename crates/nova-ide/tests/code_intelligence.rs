use nova_db::InMemoryFileStore;
use nova_ide::{completions, file_diagnostics, find_references, goto_definition};
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
