use nova_framework::{AnalyzerRegistry, CompletionContext, MemoryDatabase};
use nova_framework_micronaut::MicronautAnalyzer;
use nova_types::Span;
use std::path::Path;
use tempfile::TempDir;

fn write_file(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

/// Adapter around `MemoryDatabase` that simulates a host that cannot enumerate
/// `Database::all_files(project)` (which is optional per the framework DB contract).
struct NoAllFilesDb {
    inner: MemoryDatabase,
}

impl nova_framework::Database for NoAllFilesDb {
    fn class(&self, class: nova_types::ClassId) -> &nova_hir::framework::ClassData {
        nova_framework::Database::class(&self.inner, class)
    }

    fn project_of_class(&self, class: nova_types::ClassId) -> nova_core::ProjectId {
        nova_framework::Database::project_of_class(&self.inner, class)
    }

    fn project_of_file(&self, file: nova_core::FileId) -> nova_core::ProjectId {
        nova_framework::Database::project_of_file(&self.inner, file)
    }

    fn file_text(&self, file: nova_core::FileId) -> Option<&str> {
        nova_framework::Database::file_text(&self.inner, file)
    }

    fn file_path(&self, file: nova_core::FileId) -> Option<&std::path::Path> {
        nova_framework::Database::file_path(&self.inner, file)
    }

    fn file_id(&self, path: &std::path::Path) -> Option<nova_core::FileId> {
        nova_framework::Database::file_id(&self.inner, path)
    }

    fn all_files(&self, _project: nova_core::ProjectId) -> Vec<nova_core::FileId> {
        Vec::new()
    }

    fn has_dependency(&self, project: nova_core::ProjectId, group: &str, artifact: &str) -> bool {
        nova_framework::Database::has_dependency(&self.inner, project, group, artifact)
    }

    fn has_class_on_classpath(&self, project: nova_core::ProjectId, binary_name: &str) -> bool {
        nova_framework::Database::has_class_on_classpath(&self.inner, project, binary_name)
    }

    fn has_class_on_classpath_prefix(&self, project: nova_core::ProjectId, prefix: &str) -> bool {
        nova_framework::Database::has_class_on_classpath_prefix(&self.inner, project, prefix)
    }
}

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

#[test]
fn registry_completes_value_placeholders_without_db_file_enumeration() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().canonicalize().unwrap();

    // Ensure `nova_project::workspace_root` can find the workspace root.
    std::fs::write(root.join("pom.xml"), "<project></project>").expect("write pom.xml");

    write_file(
        &root.join("src/main/resources/application.properties"),
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

    let java_path = root.join("src/main/java/C.java");
    write_file(&java_path, java);

    let mut inner = MemoryDatabase::new();
    let project = inner.add_project();
    inner.add_dependency(project, "io.micronaut", "micronaut-runtime");

    let file = inner.add_file_with_path_and_text(project, java_path, java);

    let db = NoAllFilesDb { inner };

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
