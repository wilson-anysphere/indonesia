use nova_framework::{AnalyzerRegistry, Database, MemoryDatabase, Symbol};
use nova_framework_dagger::DaggerAnalyzer;
use nova_types::Severity;

use super::fixture_utils::load_fixture_sources;

use std::path::Path;
use tempfile::TempDir;

fn write_file(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

/// Adapter around `MemoryDatabase` that simulates a host that cannot enumerate `Database::all_files`
/// (which is optional in the framework DB surface).
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
fn registry_diagnostics_reports_missing_binding_with_correct_span() {
    let sources = load_fixture_sources("missing_binding");

    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_dependency(project, "com.google.dagger", "dagger");

    let mut foo_file = None;
    let mut component_file = None;

    for (path, text) in sources {
        let file_id = db.add_file_with_path_and_text(project, path.clone(), text);
        if path.ends_with("Foo.java") {
            foo_file = Some(file_id);
        } else if path.ends_with("AppComponent.java") {
            component_file = Some(file_id);
        }
    }

    let foo_file = foo_file.expect("Foo.java file id");
    let component_file = component_file.expect("AppComponent.java file id");

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(DaggerAnalyzer::default()));

    let diags = registry.framework_diagnostics(&db, foo_file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "DAGGER_MISSING_BINDING"),
        "expected DAGGER_MISSING_BINDING diagnostic, got: {diags:#?}"
    );

    let diag = diags
        .into_iter()
        .find(|d| d.code.as_ref() == "DAGGER_MISSING_BINDING")
        .expect("missing binding diagnostic");

    assert_eq!(diag.severity, Severity::Error);
    assert!(diag.message.contains("Missing binding"));
    assert!(diag.message.contains("Bar"));

    let span = diag.span.expect("missing binding diagnostic span");
    let text = db.file_text(foo_file).expect("Foo.java text");
    assert_eq!(
        text.get(span.start..span.end).unwrap_or(""),
        "Bar",
        "expected span to cover injected type"
    );

    // Ensure we don't leak diagnostics from other files.
    let component_diags = registry.framework_diagnostics(&db, component_file);
    assert!(
        component_diags.is_empty(),
        "expected no diagnostics for AppComponent.java, got: {component_diags:#?}"
    );
}

#[test]
fn registry_diagnostics_reports_duplicate_binding_with_correct_span() {
    let sources = load_fixture_sources("duplicate_binding");

    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_dependency(project, "com.google.dagger", "dagger");

    let mut consumer_file = None;

    for (path, text) in sources {
        let file_id = db.add_file_with_path_and_text(project, path.clone(), text);
        if path.ends_with("Consumer.java") {
            consumer_file = Some(file_id);
        }
    }

    let consumer_file = consumer_file.expect("Consumer.java file id");

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(DaggerAnalyzer::default()));

    let diags = registry.framework_diagnostics(&db, consumer_file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "DAGGER_DUPLICATE_BINDING"),
        "expected DAGGER_DUPLICATE_BINDING diagnostic, got: {diags:#?}"
    );

    let diag = diags
        .into_iter()
        .find(|d| d.code.as_ref() == "DAGGER_DUPLICATE_BINDING")
        .expect("duplicate binding diagnostic");

    assert_eq!(diag.severity, Severity::Error);
    assert!(diag.message.contains("Duplicate bindings"));
    assert!(diag.message.contains("Foo"));

    let span = diag.span.expect("duplicate binding diagnostic span");
    let text = db.file_text(consumer_file).expect("Consumer.java text");
    assert_eq!(
        text.get(span.start..span.end).unwrap_or(""),
        "Foo",
        "expected span to cover injected type"
    );
}

#[test]
fn registry_navigation_targets_include_provider_file() {
    let sources = load_fixture_sources("navigation");

    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_dependency(project, "com.google.dagger", "dagger");

    let mut consumer_file = None;
    let mut module_file = None;

    for (path, text) in sources {
        let file_id = db.add_file_with_path_and_text(project, path.clone(), text);
        if path.ends_with("Consumer.java") {
            consumer_file = Some(file_id);
        } else if path.ends_with("FooModule.java") {
            module_file = Some(file_id);
        }
    }

    let consumer_file = consumer_file.expect("Consumer.java file id");
    let module_file = module_file.expect("FooModule.java file id");

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(DaggerAnalyzer::default()));

    let targets = registry.framework_navigation_targets(&db, &Symbol::File(consumer_file));
    assert!(
        targets.iter().any(|t| t.file == module_file),
        "expected at least one navigation target into FooModule.java, got: {targets:#?}"
    );

    let target = targets
        .iter()
        .find(|t| t.file == module_file && t.label == "Provider")
        .expect("provider navigation target");
    let span = target.span.expect("provider navigation span");
    let text = db.file_text(module_file).expect("FooModule.java text");
    assert_eq!(
        text.get(span.start..span.end).unwrap_or(""),
        "provideFoo",
        "expected navigation target span to cover provider method name"
    );
}

#[test]
fn registry_diagnostics_do_not_require_db_file_enumeration() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();

    // Ensure `nova_project::workspace_root` can find the project root.
    std::fs::write(root.join("pom.xml"), "<project></project>").expect("write pom.xml");

    let mut inner = MemoryDatabase::new();
    let project = inner.add_project();
    inner.add_dependency(project, "com.google.dagger", "dagger");

    let mut foo_file = None;
    for (fixture_path, text) in load_fixture_sources("missing_binding") {
        let file_name = fixture_path.file_name().expect("fixture file name");
        let path = root.join(file_name);
        write_file(&path, &text);
        let file_id = inner.add_file_with_path_and_text(project, path.clone(), text);
        if path.ends_with("Foo.java") {
            foo_file = Some(file_id);
        }
    }
    let foo_file = foo_file.expect("Foo.java file id");

    let db = NoAllFilesDb { inner };

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(DaggerAnalyzer::default()));

    let diags = registry.framework_diagnostics(&db, foo_file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "DAGGER_MISSING_BINDING"),
        "expected DAGGER_MISSING_BINDING diagnostic, got: {diags:#?}"
    );
}

#[test]
fn registry_navigation_does_not_require_db_file_enumeration() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();

    std::fs::write(root.join("pom.xml"), "<project></project>").expect("write pom.xml");

    let mut inner = MemoryDatabase::new();
    let project = inner.add_project();
    inner.add_dependency(project, "com.google.dagger", "dagger");

    let mut consumer_file = None;
    let mut module_file = None;

    for (fixture_path, text) in load_fixture_sources("navigation") {
        let file_name = fixture_path.file_name().expect("fixture file name");
        let path = root.join(file_name);
        write_file(&path, &text);
        let file_id = inner.add_file_with_path_and_text(project, path.clone(), text);
        if path.ends_with("Consumer.java") {
            consumer_file = Some(file_id);
        } else if path.ends_with("FooModule.java") {
            module_file = Some(file_id);
        }
    }

    let consumer_file = consumer_file.expect("Consumer.java file id");
    let module_file = module_file.expect("FooModule.java file id");

    let db = NoAllFilesDb { inner };

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(DaggerAnalyzer::default()));

    let targets = registry.framework_navigation_targets(&db, &Symbol::File(consumer_file));
    assert!(
        targets.iter().any(|t| t.file == module_file),
        "expected at least one navigation target into FooModule.java, got: {targets:#?}"
    );
}
