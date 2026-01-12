use nova_db::InMemoryFileStore;
use nova_ide::{completions, file_diagnostics};
use nova_types::Severity;
use std::path::PathBuf;

use crate::framework_harness::{offset_to_position, CARET};

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

#[test]
fn micronaut_diagnostics_include_missing_bean() {
    let java_path = PathBuf::from("/workspace/src/main/java/Foo.java");
    let java_text = r#"
 import io.micronaut.context.annotation.Singleton;
import jakarta.inject.Inject;

@Singleton
class Foo {
  @Inject Missing missing;
}
"#;

    let mut db = InMemoryFileStore::new();
    let file = db.file_id_for_path(&java_path);
    db.set_file_text(file, java_text.to_string());

    let diags = file_diagnostics(&db, file);
    assert!(
        diags
            .iter()
            .any(|d| d.code == "MICRONAUT_NO_BEAN" && d.severity == Severity::Error),
        "expected MICRONAUT_NO_BEAN diagnostic, got {diags:#?}"
    );
}

#[test]
fn micronaut_value_completions_use_config_keys() {
    let config_path = PathBuf::from("/workspace/src/main/resources/application.properties");
    let java_path = PathBuf::from("/workspace/src/main/java/C.java");

    let config_text = "server.port=8080\n".to_string();
    let java_text = r#"
import io.micronaut.context.annotation.Value;
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
        "expected Micronaut config completion; got {labels:?}"
    );
}

#[test]
fn micronaut_value_completions_use_profiled_config_keys() {
    let config_path = PathBuf::from("/workspace/src/main/resources/application-test.properties");
    let java_path = PathBuf::from("/workspace/src/main/java/C.java");

    let config_text = "server.port=8080\n".to_string();
    let java_text = r#"
import io.micronaut.context.annotation.Value;
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
        "expected Micronaut config completion from profiled application*.properties; got {labels:?}"
    );
}
