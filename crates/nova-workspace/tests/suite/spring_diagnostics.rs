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
              <groupId>org.springframework</groupId>
              <artifactId>spring-context</artifactId>
              <version>6.0.0</version>
            </dependency>
          </dependencies>
        </project>
    "#
}

#[test]
fn workspace_diagnostics_include_spring_no_bean() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    write(&root.join("pom.xml"), pom_xml());
    write(
        &root.join("src/main/java/com/example/Bar.java"),
        r#"
            package com.example;

            import org.springframework.stereotype.Component;
            import org.springframework.beans.factory.annotation.Autowired;

            @Component
            class Bar {
                @Autowired
                Missing missing;
            }
        "#,
    );

    let ws = Workspace::open(root).expect("workspace open");
    let report = ws.diagnostics(root).expect("diagnostics");

    assert!(
        report
            .diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some("SPRING_NO_BEAN")),
        "expected SPRING_NO_BEAN, got: {:#?}",
        report.diagnostics
    );
}

#[test]
fn workspace_diagnostics_include_spring_ambiguous_bean() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    write(&root.join("pom.xml"), pom_xml());
    write(
        &root.join("src/main/java/com/example/FooImpl1.java"),
        r#"
            package com.example;

            import org.springframework.stereotype.Component;

            @Component
            class FooImpl1 implements Foo {
            }
        "#,
    );
    write(
        &root.join("src/main/java/com/example/FooImpl2.java"),
        r#"
            package com.example;

            import org.springframework.stereotype.Component;

            @Component
            class FooImpl2 implements Foo {
            }
        "#,
    );
    write(
        &root.join("src/main/java/com/example/Bar.java"),
        r#"
            package com.example;

            import org.springframework.stereotype.Component;
            import org.springframework.beans.factory.annotation.Autowired;

            @Component
            class Bar {
                @Autowired
                Foo foo;
            }
        "#,
    );

    let ws = Workspace::open(root).expect("workspace open");
    let report = ws.diagnostics(root).expect("diagnostics");

    assert!(
        report
            .diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some("SPRING_AMBIGUOUS_BEAN")),
        "expected SPRING_AMBIGUOUS_BEAN, got: {:#?}",
        report.diagnostics
    );
}
