use nova_framework::{AnalyzerRegistry, CompletionContext, MemoryDatabase};
use nova_framework_spring::{completion_span_for_value_placeholder, SpringAnalyzer, SPRING_NO_BEAN};

#[test]
fn applies_to_turns_on_with_spring_marker() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();

    let java = r#"
        import org.springframework.beans.factory.annotation.Autowired;
        import org.springframework.stereotype.Component;

        @Component
        class Main {
            @Autowired Missing missing;
        }

        class Missing {}
    "#;
    let file = db.add_file_with_path_and_text(project, "src/Main.java", java);

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(SpringAnalyzer::new()));

    // Without a Spring marker the analyzer should not apply.
    assert!(registry.framework_diagnostics(&db, file).is_empty());

    // Classpath marker should activate the analyzer.
    db.add_classpath_class(project, "org.springframework.context.ApplicationContext");

    let diags = registry.framework_diagnostics(&db, file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == SPRING_NO_BEAN),
        "expected SPRING_NO_BEAN; got {diags:#?}"
    );
}

#[test]
fn di_diagnostics_are_filtered_per_file() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_classpath_class(project, "org.springframework.context.ApplicationContext");

    let foo = r#"
        import org.springframework.stereotype.Component;

        @Component
        class Foo {}
    "#;
    let foo_file = db.add_file_with_path_and_text(project, "src/Foo.java", foo);

    let bar = r#"
        import org.springframework.beans.factory.annotation.Autowired;
        import org.springframework.stereotype.Component;

        @Component
        class Bar {
            @Autowired Foo foo;
            @Autowired Missing missing;
        }

        class Missing {}
    "#;
    let bar_file = db.add_file_with_path_and_text(project, "src/Bar.java", bar);

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(SpringAnalyzer::new()));

    let foo_diags = registry.framework_diagnostics(&db, foo_file);
    assert!(
        foo_diags.is_empty(),
        "expected Foo.java to have no diagnostics; got {foo_diags:#?}"
    );

    let bar_diags = registry.framework_diagnostics(&db, bar_file);
    assert_eq!(
        bar_diags.len(),
        1,
        "expected exactly one diagnostic in Bar.java (Missing), got {bar_diags:#?}"
    );
    assert_eq!(bar_diags[0].code.as_ref(), SPRING_NO_BEAN);
    assert!(
        bar_diags[0].message.contains("Missing"),
        "expected Missing to be mentioned; got {:?}",
        bar_diags[0].message
    );
    assert!(
        !bar_diags[0].message.contains("Foo"),
        "expected Foo injection to resolve via project-wide analysis; got {:?}",
        bar_diags[0].message
    );
}

#[test]
fn value_completions_include_application_properties_key_and_set_replace_span() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_classpath_class(project, "org.springframework.context.ApplicationContext");

    let config = "server.port=8080\n";
    db.add_file_with_path_and_text(
        project,
        "src/main/resources/application.properties",
        config,
    );

    let java = r#"
        import org.springframework.beans.factory.annotation.Value;
        class App {
            @Value("${server.p}")
            String port;
        }
    "#;
    let java_file = db.add_file_with_path_and_text(project, "src/App.java", java);

    let offset = java.find("${server.p}").unwrap() + "${server.p".len();
    let expected_span = completion_span_for_value_placeholder(java, offset).expect("span");

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(SpringAnalyzer::new()));

    let ctx = CompletionContext {
        project,
        file: java_file,
        offset,
    };
    let items = registry.framework_completions(&db, &ctx);

    let item = items
        .iter()
        .find(|i| i.label == "server.port")
        .expect("expected server.port completion item");
    assert_eq!(item.replace_span, Some(expected_span));
}

