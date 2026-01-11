use nova_workspace::Workspace;
use std::fs;
use std::path::Path;

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("mkdir");
    }
    fs::write(path, contents).expect("write");
}

fn pom_xml() -> &'static str {
    r#"
        <project xmlns="http://maven.apache.org/POM/4.0.0">
          <modelVersion>4.0.0</modelVersion>
          <groupId>com.example</groupId>
          <artifactId>demo</artifactId>
          <version>0.0.1</version>
          <dependencies>
            <dependency>
              <groupId>jakarta.persistence</groupId>
              <artifactId>jakarta.persistence-api</artifactId>
              <version>3.1.0</version>
            </dependency>
          </dependencies>
        </project>
    "#
}

#[test]
fn workspace_diagnostics_include_jpa_missing_id_and_jpql_errors() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    write(&root.join("pom.xml"), pom_xml());
    write(
        &root.join("src/main/java/com/example/User.java"),
        r#"
            package com.example;

            import jakarta.persistence.Entity;

            @Entity
            class User {
                private String name;
            }
        "#,
    );
    write(
        &root.join("src/main/java/com/example/Repo.java"),
        r#"
            package com.example;

            import org.springframework.data.jpa.repository.Query;

            interface Repo {
                @Query("SELECT u FROM Unknown u")
                void load();
            }
        "#,
    );

    let ws = Workspace::open(root).expect("workspace open");
    let report = ws.diagnostics(root).expect("diagnostics");

    assert!(
        report
            .diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some("JPA_MISSING_ID")),
        "expected JPA_MISSING_ID, got: {:#?}",
        report.diagnostics
    );
    assert!(
        report
            .diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some("JPQL_UNKNOWN_ENTITY")),
        "expected JPQL_UNKNOWN_ENTITY, got: {:#?}",
        report.diagnostics
    );
}
