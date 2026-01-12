use std::sync::Arc;

use nova_config::NovaConfig;
use nova_db::InMemoryFileStore;
use nova_ext::ProjectId;
use nova_framework_quarkus::CDI_UNSATISFIED_CODE;
use nova_scheduler::CancellationToken;
use tempfile::TempDir;

use nova_ide::extensions::IdeExtensions;

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
fn framework_diagnostics_not_duplicated_when_build_metadata_is_available() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("project");
    write_quarkus_pom(&root);

    // Unsatisfied CDI injection (Quarkus).
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

    let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(db);
    let ide = IdeExtensions::<dyn nova_db::Database + Send + Sync>::with_default_registry(
        db,
        Arc::new(NovaConfig::default()),
        ProjectId::new(0),
    );

    let diags = ide.diagnostics(CancellationToken::new(), file);
    let count = diags
        .iter()
        .filter(|d| d.code.as_ref() == CDI_UNSATISFIED_CODE)
        .count();

    assert_eq!(
        count, 1,
        "expected exactly one {CDI_UNSATISFIED_CODE} diagnostic; got {diags:#?}"
    );
}
