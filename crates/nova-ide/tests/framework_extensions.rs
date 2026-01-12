mod framework_harness;

use std::path::{Path, PathBuf};

use framework_harness::{fixture_multi, ide_with_default_registry};
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
fn micronaut_value_completions_are_surfaced_via_ide_extensions() {
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
}

#[test]
fn quarkus_diagnostics_are_surfaced_via_ide_extensions() {
    let java_path = PathBuf::from("/quarkus/src/main/java/com/example/ServiceA.java");
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

    let db: std::sync::Arc<dyn nova_db::Database + Send + Sync> = std::sync::Arc::new(db);
    let ide = ide_with_default_registry(std::sync::Arc::clone(&db));

    let diags = ide.all_diagnostics(CancellationToken::new(), foo_file);
    assert!(
        diags.iter().any(|d| d.code == "DAGGER_MISSING_BINDING"),
        "expected Dagger missing binding diagnostic; got {diags:#?}"
    );
}
