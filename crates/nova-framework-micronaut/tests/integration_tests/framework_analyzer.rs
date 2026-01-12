use nova_framework::{AnalyzerRegistry, CompletionContext, MemoryDatabase};
use nova_framework_micronaut::MicronautAnalyzer;
use nova_types::Span;

#[test]
fn registry_emits_missing_bean_diagnostic_for_correct_file() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_dependency(project, "io.micronaut", "micronaut-runtime");

    let foo = db.add_file_with_path_and_text(
        project,
        "src/Foo.java",
        r#"
            import io.micronaut.context.annotation.Singleton;
            import jakarta.inject.Inject;

            @Singleton
            class Foo {
                @Inject Bar bar;
            }
        "#,
    );
    let ok = db.add_file_with_path_and_text(
        project,
        "src/Ok.java",
        r#"
            class Ok {}
        "#,
    );

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(MicronautAnalyzer::new()));

    let foo_diags = registry.framework_diagnostics(&db, foo);
    assert_eq!(foo_diags.len(), 1);
    assert_eq!(foo_diags[0].code.as_ref(), "MICRONAUT_NO_BEAN");

    let ok_diags = registry.framework_diagnostics(&db, ok);
    assert!(ok_diags.is_empty(), "unexpected diagnostics: {ok_diags:#?}");
}

#[test]
fn registry_emits_missing_bean_diagnostic_for_pathless_java_file() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_dependency(project, "io.micronaut", "micronaut-runtime");

    let java = r#"
        import io.micronaut.context.annotation.Singleton;
        import jakarta.inject.Inject;

        @Singleton
        class Foo {
            @Inject Bar bar;
        }
    "#;
    let file = db.add_file_with_text(project, java);

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(MicronautAnalyzer::new()));

    let diags = registry.framework_diagnostics(&db, file);
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0].code.as_ref(), "MICRONAUT_NO_BEAN");
}

#[test]
fn registry_completes_value_placeholders_from_application_profile_properties() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_dependency(project, "io.micronaut", "micronaut-runtime");

    db.add_file_with_path_and_text(
        project,
        "src/main/resources/application-test.properties",
        "app.name=Nova\napp.number=7\n",
    );

    let java = r#"
        import io.micronaut.context.annotation.Value;

        class C {
            @Value("${app.n}")
            String value;
        }
    "#;
    let placeholder_start = java.find("${app.n").expect("placeholder missing");
    let cursor = placeholder_start + "${app.n".len();

    let file = db.add_file_with_path_and_text(project, "src/C.java", java);

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(MicronautAnalyzer::new()));

    let ctx = CompletionContext {
        project,
        file,
        offset: cursor,
    };

    let items = registry.framework_completions(&db, &ctx);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(labels.contains(&"app.name"), "labels={labels:?}");
    assert!(labels.contains(&"app.number"), "labels={labels:?}");

    let item = items
        .iter()
        .find(|i| i.label == "app.name")
        .expect("app.name completion missing");
    assert_eq!(
        item.replace_span,
        Some(Span::new(placeholder_start + 2, cursor)),
        "expected completion to replace the current key prefix"
    );
}

#[test]
fn registry_completes_value_placeholders_from_application_properties_for_pathless_java_file() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_dependency(project, "io.micronaut", "micronaut-runtime");

    db.add_file_with_path_and_text(
        project,
        "src/main/resources/application.properties",
        "app.name=Nova\napp.number=7\n",
    );

    let java = r#"
        import io.micronaut.context.annotation.Value;

        class C {
            @Value("${app.n}")
            String value;
        }
    "#;
    let placeholder_start = java.find("${app.n").expect("placeholder missing");
    let cursor = placeholder_start + "${app.n".len();

    let file = db.add_file_with_text(project, java);

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(MicronautAnalyzer::new()));

    let ctx = CompletionContext {
        project,
        file,
        offset: cursor,
    };

    let items = registry.framework_completions(&db, &ctx);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(labels.contains(&"app.name"), "labels={labels:?}");
    assert!(labels.contains(&"app.number"), "labels={labels:?}");

    let item = items
        .iter()
        .find(|i| i.label == "app.name")
        .expect("app.name completion missing");
    assert_eq!(
        item.replace_span,
        Some(Span::new(placeholder_start + 2, cursor)),
        "expected completion to replace the current key prefix"
    );
}
