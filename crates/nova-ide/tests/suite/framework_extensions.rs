use std::path::{Path, PathBuf};

use crate::framework_harness::{fixture_multi, ide_with_default_registry};
use lsp_types::CompletionTextEdit;
use nova_db::{Database as _, InMemoryFileStore};
use nova_framework_quarkus::CDI_UNSATISFIED_CODE;
use nova_framework_spring::SPRING_NO_BEAN;
use nova_scheduler::CancellationToken;
use nova_types::Severity;

fn collect_java_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read fixture dir") {
        let entry = entry.expect("read entry");
        let path = entry.path();
        if path.is_dir() {
            collect_java_files(&path, out);
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("java") {
            continue;
        }
        out.push(path);
    }
}

#[test]
fn spring_diagnostics_are_surfaced_via_ide_extensions() {
    let java_path = PathBuf::from("/spring-missing/src/main/java/A.java");
    let java_text = r#"import org.springframework.beans.factory.annotation.Autowired;
import org.springframework.stereotype.Component;

@Component
class A {
  @Autowired Missing missing;
}

class Missing {}
"#;

    let fixture = fixture_multi(java_path, java_text, vec![]);
    let diags = fixture
        .ide
        .all_diagnostics(CancellationToken::new(), fixture.file);

    assert!(
        diags
            .iter()
            .any(|d| d.code == SPRING_NO_BEAN && d.severity == Severity::Error),
        "expected Spring missing-bean diagnostic; got {diags:#?}"
    );
}

#[test]
fn jpql_completions_are_surfaced_via_ide_extensions() {
    let user_path = PathBuf::from("/workspace/src/main/java/com/example/User.java");
    let post_path = PathBuf::from("/workspace/src/main/java/com/example/Post.java");
    let repo_path = PathBuf::from("/workspace/src/main/java/com/example/UserRepository.java");

    let user_source = r#"package com.example;
import jakarta.persistence.Entity;
import jakarta.persistence.Id;

@Entity
class User {
  @Id Long id;
  String name;
}
"#
    .to_string();

    let post_source = r#"package com.example;
import jakarta.persistence.Entity;
import jakarta.persistence.Id;

@Entity
class Post {
  @Id Long id;
  String title;
}
"#
    .to_string();

    let repo_text = r#"package com.example;
import org.springframework.data.jpa.repository.Query;

class UserRepository {
  @Query("SELECT u FROM User u WHERE u.<|>")
  void m();
}
"#;

    let fixture = fixture_multi(
        repo_path,
        repo_text,
        vec![(user_path, user_source), (post_path, post_source)],
    );

    let items =
        fixture
            .ide
            .completions_lsp(CancellationToken::new(), fixture.file, fixture.position);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();

    assert!(
        labels.contains(&"id"),
        "expected JPQL completions to include `id`; got {labels:?}"
    );
    assert!(
        labels.contains(&"name"),
        "expected JPQL completions to include `name`; got {labels:?}"
    );
}

#[test]
fn annotation_attribute_completions_are_surfaced_via_ide_extensions() {
    let anno_path = PathBuf::from("/workspace/src/main/java/p/MyAnno.java");
    let java_path = PathBuf::from("/workspace/src/main/java/p/Main.java");

    let anno_text = r#"package p; public @interface MyAnno { String value(); int count(); }"#;
    let java_text = r#"package p; @MyAnno(co<|>) class Main {}"#;

    let fixture = fixture_multi(
        java_path,
        java_text,
        vec![(anno_path, anno_text.to_string())],
    );

    let items =
        fixture
            .ide
            .completions_lsp(CancellationToken::new(), fixture.file, fixture.position);

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
fn spring_value_completions_replace_full_placeholder_prefix_via_ide_extensions() {
    use crate::framework_harness::offset_to_position;

    let config_path = PathBuf::from("/spring-value/src/main/resources/application.properties");
    let java_path = PathBuf::from("/spring-value/src/main/java/C.java");

    let config_text = "server.port=8080\n".to_string();
    let java_text = r#"import org.springframework.beans.factory.annotation.Value;
class C {
  @Value("${server.p<|>}")
  String port;
}
"#;

    let fixture = fixture_multi(java_path, java_text, vec![(config_path, config_text)]);
    let items =
        fixture
            .ide
            .completions_lsp(CancellationToken::new(), fixture.file, fixture.position);
    let item = items
        .iter()
        .find(|item| item.label == "server.port")
        .expect("expected Spring completion item");

    let edit = match item.text_edit.as_ref().expect("expected text_edit") {
        CompletionTextEdit::Edit(edit) => edit,
        other => panic!("unexpected text_edit variant: {other:?}"),
    };

    let key_start = fixture
        .text
        .find("server.p")
        .expect("expected placeholder prefix in fixture");

    assert_eq!(
        edit.range.start,
        offset_to_position(&fixture.text, key_start)
    );
    assert_eq!(edit.range.end, fixture.position);
}

#[test]
fn spring_properties_key_completions_replace_full_key_prefix_via_ide_extensions() {
    use crate::framework_harness::offset_to_position;

    let config_path = PathBuf::from("/spring-props/src/main/resources/application.properties");
    let java_path = PathBuf::from("/spring-props/src/main/java/Dummy.java");

    let config_text = r#"server.port=8080
server.p<|>
"#;
    let java_text = "import org.springframework.stereotype.Component;\n@Component class Dummy {}\n";

    let fixture = fixture_multi(
        config_path,
        config_text,
        vec![(java_path, java_text.to_string())],
    );

    let items =
        fixture
            .ide
            .completions_lsp(CancellationToken::new(), fixture.file, fixture.position);
    let item = items
        .iter()
        .find(|item| item.label == "server.port")
        .expect("expected Spring completion item");

    let edit = match item.text_edit.as_ref().expect("expected text_edit") {
        CompletionTextEdit::Edit(edit) => edit,
        other => panic!("unexpected text_edit variant: {other:?}"),
    };

    let key_start = fixture
        .text
        .rfind("server.p")
        .expect("expected server.p key in fixture");
    assert_eq!(
        edit.range.start,
        offset_to_position(&fixture.text, key_start)
    );
    assert_eq!(edit.range.end, fixture.position);
}

#[test]
fn spring_yaml_key_completions_replace_full_segment_prefix_via_ide_extensions() {
    use crate::framework_harness::offset_to_position;

    let config_path = PathBuf::from("/spring-yaml/src/main/resources/application.yml");
    let java_path = PathBuf::from("/spring-yaml/src/main/java/Dummy.java");

    let config_text = r#"spring:
  main:
    banner-mode: console
    banner-m<|>
"#;
    let java_text = "import org.springframework.stereotype.Component;\n@Component class Dummy {}\n";

    let fixture = fixture_multi(
        config_path,
        config_text,
        vec![(java_path, java_text.to_string())],
    );

    let items =
        fixture
            .ide
            .completions_lsp(CancellationToken::new(), fixture.file, fixture.position);
    let item = items
        .iter()
        .find(|item| item.label == "banner-mode")
        .expect("expected Spring completion item");

    let edit = match item.text_edit.as_ref().expect("expected text_edit") {
        CompletionTextEdit::Edit(edit) => edit,
        other => panic!("unexpected text_edit variant: {other:?}"),
    };

    let key_start = fixture
        .text
        .rfind("banner-m")
        .expect("expected banner-m key in fixture");
    assert_eq!(
        edit.range.start,
        offset_to_position(&fixture.text, key_start)
    );
    assert_eq!(edit.range.end, fixture.position);
}

#[test]
fn micronaut_value_completions_are_surfaced_via_ide_extensions() {
    use crate::framework_harness::offset_to_position;

    let config_path = PathBuf::from("/micronaut-value/src/main/resources/application.properties");
    let java_path = PathBuf::from("/micronaut-value/src/main/java/com/example/A.java");

    let config_text = "greeting.message=Hello\n".to_string();
    let java_text = r#"import io.micronaut.context.annotation.Value;

class A {
  @Value("${greeting.<|>}")
  String msg;
}
"#;

    let fixture = fixture_multi(java_path, java_text, vec![(config_path, config_text)]);

    let items =
        fixture
            .ide
            .completions_lsp(CancellationToken::new(), fixture.file, fixture.position);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"greeting.message"),
        "expected Micronaut @Value completions to include greeting.message; got {labels:?}"
    );

    let item = items
        .iter()
        .find(|item| item.label == "greeting.message")
        .expect("expected Micronaut completion item");
    let edit = match item.text_edit.as_ref().expect("expected text_edit") {
        CompletionTextEdit::Edit(edit) => edit,
        other => panic!("unexpected text_edit variant: {other:?}"),
    };
    let key_start = fixture
        .text
        .find("greeting.")
        .expect("expected placeholder prefix in fixture");
    assert_eq!(
        edit.range.start,
        offset_to_position(&fixture.text, key_start)
    );
    assert_eq!(edit.range.end, fixture.position);
}

#[test]
fn quarkus_diagnostics_are_surfaced_via_ide_extensions() {
    // Use a temp root instead of a fixed absolute path so the test does not accidentally pick up
    // build metadata from the host filesystem (e.g. `/quarkus/pom.xml`), which would route
    // diagnostics through analyzer-based providers instead of the source-heuristic framework cache.
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let root = tmp.path().join("quarkus-fixture");
    std::fs::create_dir_all(root.join("src")).expect("create fixture src dir");

    let java_path = root.join("src/main/java/com/example/ServiceA.java");
    let java_text = r#"import io.quarkus.runtime.Startup;
 import jakarta.enterprise.context.ApplicationScoped;
 import jakarta.inject.Inject;

@Startup
@ApplicationScoped
class ServiceA {
  @Inject ServiceB missing;
}
"#;

    let fixture = fixture_multi(java_path, java_text, vec![]);
    let diags = fixture
        .ide
        .all_diagnostics(CancellationToken::new(), fixture.file);

    assert!(
        diags.iter().any(|d| d.code == CDI_UNSATISFIED_CODE),
        "expected Quarkus CDI diagnostic; got {diags:#?}"
    );
}

#[test]
fn dagger_diagnostics_are_surfaced_via_ide_extensions() {
    let fixtures_root =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../nova-framework-dagger/tests/fixtures");
    let root = fixtures_root.join("missing_binding");

    let mut paths = Vec::new();
    collect_java_files(&root, &mut paths);
    paths.sort();

    let mut db = InMemoryFileStore::new();
    for path in &paths {
        let text = std::fs::read_to_string(path).expect("read java fixture file");
        let id = db.file_id_for_path(path);
        db.set_file_text(id, text);
    }

    let foo_path = paths
        .iter()
        .find(|p| p.ends_with("Foo.java"))
        .expect("Foo.java fixture path");
    let foo_file = db.file_id(foo_path).expect("Foo.java file id");

    let db = std::sync::Arc::new(db);
    let ide = ide_with_default_registry(std::sync::Arc::clone(&db));

    let diags = ide.all_diagnostics(CancellationToken::new(), foo_file);
    assert!(
        diags.iter().any(|d| d.code == "DAGGER_MISSING_BINDING"),
        "expected Dagger missing binding diagnostic; got {diags:#?}"
    );
}

#[test]
fn java_import_completions_are_surfaced_via_ide_extensions() {
    use crate::framework_harness::offset_to_position;

    let java_path = PathBuf::from("/imports/src/main/java/A.java");
    let java_text = r#"
import java.u<|>;
class A {}
"#;

    let fixture = fixture_multi(java_path, java_text, vec![]);
    let items =
        fixture
            .ide
            .completions_lsp(CancellationToken::new(), fixture.file, fixture.position);
    let item = items
        .iter()
        .find(|i| i.label == "util" || i.label == "util.")
        .expect("expected java.util package completion via ide_extensions");

    let u_offset = fixture
        .text
        .find("java.u")
        .expect("expected java.u in fixture")
        + "java.".len();

    let edit = match item.text_edit.as_ref().expect("expected text_edit") {
        CompletionTextEdit::Edit(edit) => edit,
        other => panic!("unexpected text_edit variant: {other:?}"),
    };

    assert_eq!(
        edit.range.start,
        offset_to_position(&fixture.text, u_offset)
    );
    assert_eq!(edit.range.end, fixture.position);
}
