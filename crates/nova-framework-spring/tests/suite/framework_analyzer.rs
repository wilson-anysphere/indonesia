use nova_framework::{AnalyzerRegistry, CompletionContext, Database, MemoryDatabase, Symbol};
use nova_framework_spring::{SpringAnalyzer, SPRING_NO_BEAN, SPRING_UNKNOWN_CONFIG_KEY};
use nova_types::Span;

#[test]
fn di_diagnostics_are_scoped_to_the_current_file() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_dependency(project, "org.springframework", "spring-context");

    let foo = db.add_file_with_path_and_text(
        project,
        "src/Foo.java",
        r#"
            import org.springframework.stereotype.Component;

            @Component
            class Foo {}
        "#,
    );

    let bar = db.add_file_with_path_and_text(
        project,
        "src/Bar.java",
        r#"
            import org.springframework.stereotype.Component;
            import org.springframework.beans.factory.annotation.Autowired;

            @Component
            class Bar {
                @Autowired
                Missing missing;
            }
        "#,
    );

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(SpringAnalyzer::new()));

    let foo_diags = registry.framework_diagnostics(&db, foo);
    assert!(
        foo_diags.is_empty(),
        "expected no diagnostics for Foo.java; got {foo_diags:?}"
    );

    let bar_diags = registry.framework_diagnostics(&db, bar);
    assert_eq!(bar_diags.len(), 1);
    assert_eq!(bar_diags[0].code.as_ref(), SPRING_NO_BEAN);
    assert!(bar_diags[0].message.contains("Missing"));
}

#[test]
fn config_diagnostics_report_unknown_keys() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_dependency(project, "org.springframework", "spring-context");

    db.add_file_with_path_and_text(
        project,
        "spring-configuration-metadata.json",
        r#"
        {
          "properties": [
            { "name": "server.port", "type": "java.lang.Integer" }
          ]
        }
        "#,
    );

    let config = db.add_file_with_path_and_text(
        project,
        "src/main/resources/application.properties",
        "server.port=8080\nunknown.key=foo\n",
    );

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(SpringAnalyzer::new()));

    let diags = registry.framework_diagnostics(&db, config);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == SPRING_UNKNOWN_CONFIG_KEY
                && d.message.contains("unknown.key")),
        "expected SPRING_UNKNOWN_CONFIG_KEY for unknown.key; got {diags:?}"
    );
}

#[test]
fn config_diagnostics_are_case_insensitive_for_application_properties() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_dependency(project, "org.springframework", "spring-context");

    // Ensure metadata ingestion works even when the metadata file name casing is different.
    db.add_file_with_path_and_text(
        project,
        "SPRING-CONFIGURATION-METADATA.JSON",
        r#"
        {
          "properties": [
            { "name": "server.port", "type": "java.lang.Integer" }
          ]
        }
        "#,
    );

    // Non-standard casing for the application config file name and extension.
    let config = db.add_file_with_path_and_text(
        project,
        "src/main/resources/Application.PROPERTIES",
        "unknown.key=foo\n",
    );

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(SpringAnalyzer::new()));

    let diags = registry.framework_diagnostics(&db, config);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == SPRING_UNKNOWN_CONFIG_KEY
                && d.message.contains("unknown.key")),
        "expected SPRING_UNKNOWN_CONFIG_KEY for unknown.key; got {diags:?}"
    );
}

#[test]
fn value_placeholder_completion_includes_replace_span() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_dependency(project, "org.springframework", "spring-context");

    db.add_file_with_path_and_text(
        project,
        "spring-configuration-metadata.json",
        r#"
        {
          "properties": [
            { "name": "server.port", "type": "java.lang.Integer" }
          ]
        }
        "#,
    );

    let java = r#"
        import org.springframework.beans.factory.annotation.Value;

        class C {
            @Value("${ser}")
            String port;
        }
    "#;

    let file = db.add_file_with_path_and_text(project, "src/C.java", java);

    let placeholder_start = java.find("${ser}").expect("placeholder");
    let offset = placeholder_start + "${ser".len();
    let expected_replace_span = Span::new(placeholder_start + 2, offset);

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(SpringAnalyzer::new()));

    let ctx = CompletionContext {
        project,
        file,
        offset,
    };
    let items = registry.framework_completions(&db, &ctx);

    let server_port = items
        .iter()
        .find(|i| i.label == "server.port")
        .expect("expected server.port completion");
    assert_eq!(server_port.replace_span, Some(expected_replace_span));
}

#[test]
fn value_placeholder_completion_works_without_file_path() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_dependency(project, "org.springframework", "spring-context");

    db.add_file_with_path_and_text(
        project,
        "spring-configuration-metadata.json",
        r#"
        {
          "properties": [
            { "name": "server.port", "type": "java.lang.Integer" }
          ]
        }
        "#,
    );

    let java = r#"
        import org.springframework.beans.factory.annotation.Value;

        class C {
            @Value("${ser}")
            String port;
        }
    "#;

    // No file path information for this file (simulates virtual buffers).
    let file = db.add_file_with_text(project, java);

    let placeholder_start = java.find("${ser}").expect("placeholder");
    let offset = placeholder_start + "${ser".len();

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(SpringAnalyzer::new()));

    let ctx = CompletionContext {
        project,
        file,
        offset,
    };
    let items = registry.framework_completions(&db, &ctx);
    assert!(
        items.iter().any(|i| i.label == "server.port"),
        "expected server.port completion; got {items:?}"
    );
}

#[test]
fn di_diagnostics_work_without_file_path() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_dependency(project, "org.springframework", "spring-context");

    let java = r#"
        import org.springframework.stereotype.Component;
        import org.springframework.beans.factory.annotation.Autowired;

        @Component
        class C {
            @Autowired Missing missing;
        }

        class Missing {}
    "#;

    let file = db.add_file_with_text(project, java);

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(SpringAnalyzer::new()));

    let diags = registry.framework_diagnostics(&db, file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == SPRING_NO_BEAN),
        "expected SPRING_NO_BEAN diagnostic; got {diags:?}"
    );
}

#[test]
fn di_navigation_targets_include_bean_definition_file() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_dependency(project, "org.springframework", "spring-context");

    let foo = db.add_file_with_path_and_text(
        project,
        "src/Foo.java",
        r#"
            import org.springframework.stereotype.Component;

            @Component
            class Foo {}
        "#,
    );

    let bar = db.add_file_with_path_and_text(
        project,
        "src/Bar.java",
        r#"
            import org.springframework.stereotype.Component;
            import org.springframework.beans.factory.annotation.Autowired;

            @Component
            class Bar {
                @Autowired Foo foo;
            }
        "#,
    );

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(SpringAnalyzer::new()));

    let targets = registry.framework_navigation_targets(&db, &Symbol::File(bar));
    assert!(
        targets
            .iter()
            .any(|t| t.file == foo && t.label == "Bean: foo"),
        "expected Bean: foo navigation target into Foo.java; got {targets:?}"
    );

    let target = targets
        .iter()
        .find(|t| t.file == foo && t.label == "Bean: foo")
        .expect("Bean: foo navigation target");
    let span = target.span.expect("navigation target span");
    let text = db.file_text(foo).expect("Foo.java text");
    assert_eq!(
        text.get(span.start..span.end).unwrap_or(""),
        "Foo",
        "expected navigation span to cover bean class name"
    );
}

#[test]
fn di_navigation_targets_include_injection_file() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_dependency(project, "org.springframework", "spring-context");

    let foo = db.add_file_with_path_and_text(
        project,
        "src/Foo.java",
        r#"
            import org.springframework.stereotype.Component;

            @Component
            class Foo {}
        "#,
    );

    let bar = db.add_file_with_path_and_text(
        project,
        "src/Bar.java",
        r#"
            import org.springframework.stereotype.Component;
            import org.springframework.beans.factory.annotation.Autowired;

            @Component
            class Bar {
                @Autowired Foo foo;
            }
        "#,
    );

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(SpringAnalyzer::new()));

    let targets = registry.framework_navigation_targets(&db, &Symbol::File(foo));
    assert!(
        targets
            .iter()
            .any(|t| t.file == bar && t.label == "Injected into Bar"),
        "expected Injected into Bar navigation target into Bar.java; got {targets:?}"
    );

    let target = targets
        .iter()
        .find(|t| t.file == bar && t.label == "Injected into Bar")
        .expect("Injected into Bar navigation target");
    let span = target.span.expect("navigation target span");
    let text = db.file_text(bar).expect("Bar.java text");
    assert_eq!(
        text.get(span.start..span.end).unwrap_or(""),
        "foo",
        "expected navigation span to cover injection field name"
    );
}
