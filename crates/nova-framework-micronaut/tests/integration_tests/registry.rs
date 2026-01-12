use nova_framework::{AnalyzerRegistry, CompletionContext, MemoryDatabase};
use nova_framework_micronaut::MicronautAnalyzer;
use nova_types::Span;

#[test]
fn registry_reports_missing_bean_diagnostic_on_correct_file() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_dependency(project, "io.micronaut", "micronaut-runtime");

    let foo = r#"
        import io.micronaut.context.annotation.Singleton;
        import jakarta.inject.Inject;

        @Singleton
        class Foo {
            @Inject Bar bar;
        }
    "#;
    let other = r#"class Other {}"#;

    let foo_file = db.add_file_with_path_and_text(project, "src/main/java/Foo.java", foo);
    let other_file = db.add_file_with_path_and_text(project, "src/main/java/Other.java", other);

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(MicronautAnalyzer::new()));

    let foo_diags = registry.framework_diagnostics(&db, foo_file);
    assert_eq!(foo_diags.len(), 1, "unexpected diagnostics: {foo_diags:#?}");
    assert_eq!(foo_diags[0].code.as_ref(), "MICRONAUT_NO_BEAN");

    let other_diags = registry.framework_diagnostics(&db, other_file);
    assert!(
        other_diags.is_empty(),
        "expected no diagnostics for other file, got: {other_diags:#?}"
    );
}

#[test]
fn registry_completes_value_placeholder_from_application_properties() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_dependency(project, "io.micronaut", "micronaut-runtime");

    db.add_file_with_path_and_text(
        project,
        "src/main/resources/application.properties",
        "foo.bar=1\nmicronaut.server.port=8080\n",
    );

    let java = r#"
        import io.micronaut.context.annotation.Value;

        class C {
            @Value("${foo.}")
            String value;
        }
    "#;
    let java_file = db.add_file_with_path_and_text(project, "src/main/java/C.java", java);

    let placeholder_start = java.find("${foo.").expect("placeholder");
    let offset = placeholder_start + "${foo.".len();

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(MicronautAnalyzer::new()));

    let ctx = CompletionContext {
        project,
        file: java_file,
        offset,
    };

    let items = registry.framework_completions(&db, &ctx);
    let foo_bar = items
        .iter()
        .find(|item| item.label == "foo.bar")
        .expect("expected foo.bar completion");

    assert_eq!(
        foo_bar.replace_span,
        Some(Span::new(placeholder_start + 2, offset)),
        "expected completion to replace current key prefix"
    );
}

