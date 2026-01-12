use nova_db::InMemoryFileStore;
use nova_framework_quarkus::CDI_UNSATISFIED_CODE;
use nova_ide::{completions, file_diagnostics};
use nova_types::Severity;
use tempfile::TempDir;

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

fn write_quarkus_pom(root: &std::path::Path) {
    std::fs::create_dir_all(root).unwrap();
    std::fs::write(
        root.join("pom.xml"),
        r#"
            <project xmlns="http://maven.apache.org/POM/4.0.0"
                     xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"
                     xsi:schemaLocation="http://maven.apache.org/POM/4.0.0 http://maven.apache.org/xsd/maven-4.0.0.xsd">
              <modelVersion>4.0.0</modelVersion>
              <groupId>com.example</groupId>
              <artifactId>demo</artifactId>
              <version>1.0.0</version>
              <dependencies>
                <dependency>
                  <groupId>io.quarkus</groupId>
                  <artifactId>quarkus-arc</artifactId>
                  <version>3.0.0</version>
                </dependency>
              </dependencies>
            </project>
        "#,
    )
    .unwrap();
}

#[test]
fn quarkus_cdi_diagnostics_include_spans() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("project");
    write_quarkus_pom(&root);

    let src = r#"
        import jakarta.enterprise.context.ApplicationScoped;
        import jakarta.inject.Inject;

        @ApplicationScoped
        public class ServiceA {
          @Inject ServiceB missing;
        }
    "#;

    let mut db = InMemoryFileStore::new();
    let path = root.join("src/main/java/com/example/ServiceA.java");
    let file = db.file_id_for_path(&path);
    db.set_file_text(file, src.to_string());

    let diags = file_diagnostics(&db, file);
    let diag = diags
        .iter()
        .find(|d| d.code == CDI_UNSATISFIED_CODE)
        .expect("expected Quarkus CDI unsatisfied dependency diagnostic");

    assert_eq!(diag.severity, Severity::Error);
    let span = diag.span.expect("expected diagnostic span");
    assert_eq!(&src[span.start..span.end], "missing");
}

#[test]
fn quarkus_config_property_completion_uses_application_properties() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("project");
    write_quarkus_pom(&root);

    let config_path = root.join("src/main/resources/application.properties");
    let config_text = "quarkus.http.port=8080\n".to_string();

    let java_path = root.join("src/main/java/com/example/C.java");
    let java_text_with_caret = r#"
        import org.eclipse.microprofile.config.inject.ConfigProperty;

        class C {
          @ConfigProperty(name="quarkus.ht<|>")
          String port;
        }
    "#;

    let caret = "<|>";
    let caret_offset = java_text_with_caret
        .find(caret)
        .expect("fixture must contain <|> caret marker");
    let java_text = java_text_with_caret.replace(caret, "");
    let pos = offset_to_position(&java_text, caret_offset);

    let mut db = InMemoryFileStore::new();
    let cfg_file = db.file_id_for_path(&config_path);
    db.set_file_text(cfg_file, config_text);
    let java_file = db.file_id_for_path(&java_path);
    db.set_file_text(java_file, java_text);

    let items = completions(&db, java_file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"quarkus.http.port"),
        "expected Quarkus config completion; got {labels:?}"
    );
}

#[test]
fn quarkus_cdi_diagnostics_are_stable_across_requests() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("project");
    write_quarkus_pom(&root);

    let src = r#"
        import jakarta.enterprise.context.ApplicationScoped;
        import jakarta.inject.Inject;

        @ApplicationScoped
        public class ServiceA {
          @Inject ServiceB missing;
        }
    "#;

    let mut db = InMemoryFileStore::new();
    let path = root.join("src/main/java/com/example/ServiceA.java");
    let file = db.file_id_for_path(&path);
    db.set_file_text(file, src.to_string());

    let first = file_diagnostics(&db, file);
    let second = file_diagnostics(&db, file);

    assert_eq!(first, second);
}
