use std::sync::Arc;

use nova_config::NovaConfig;
use nova_db::InMemoryFileStore;
use nova_ext::ProjectId;
use nova_framework_quarkus::CDI_UNSATISFIED_CODE;
use nova_ide::extensions::{IdeExtensions, FRAMEWORK_ANALYZER_REGISTRY_PROVIDER_ID};
use nova_scheduler::CancellationToken;
use tempfile::TempDir;

#[test]
fn default_registry_does_not_duplicate_quarkus_framework_diagnostics_when_project_config_is_available(
) {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path().join("project");
    std::fs::create_dir_all(&root).expect("mkdir project");

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
                  <version>1.0.0</version>
                </dependency>
              </dependencies>
            </project>
        "#,
    )
    .expect("write pom.xml");

    let src_dir = root.join("src/main/java/com/example");
    std::fs::create_dir_all(&src_dir).expect("mkdir java package dir");
    let java_path = src_dir.join("ServiceA.java");

    // Ensure the file path exists on disk so `nova_project::workspace_root` prefers build markers
    // (i.e. `pom.xml`) rather than the in-memory fallback heuristics.
    std::fs::write(&java_path, "").expect("touch java file");

    let java_text = r#"
        import jakarta.enterprise.context.ApplicationScoped;
        import jakarta.inject.Inject;

        @ApplicationScoped
        public class ServiceA {
          @Inject ServiceB missing;
        }
    "#;

    let mut db = InMemoryFileStore::new();
    let file = db.file_id_for_path(&java_path);
    db.set_file_text(file, java_text.to_string());

    let db = Arc::new(db);
    let ide = IdeExtensions::with_default_registry(
        db,
        Arc::new(NovaConfig::default()),
        ProjectId::new(0),
    );

    let diags = ide.diagnostics(CancellationToken::new(), file);
    let matching = diags
        .iter()
        .filter(|d| d.code.as_ref() == CDI_UNSATISFIED_CODE)
        .collect::<Vec<_>>();

    assert_eq!(
        matching.len(),
        1,
        "expected exactly one Quarkus CDI unsatisfied dependency diagnostic; got {matching:#?}\n\nall diagnostics:\n{diags:#?}"
    );

    let stats = ide.registry().stats();
    assert!(
        stats.diagnostic.contains_key("nova.framework.diagnostics"),
        "expected default registry to contain the legacy framework diagnostics provider id"
    );
    assert!(
        stats
            .diagnostic
            .contains_key(FRAMEWORK_ANALYZER_REGISTRY_PROVIDER_ID),
        "expected default registry to contain the framework analyzer registry provider id"
    );
}

