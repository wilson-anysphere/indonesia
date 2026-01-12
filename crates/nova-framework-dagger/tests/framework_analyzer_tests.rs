use std::path::{Path, PathBuf};

use nova_framework::{AnalyzerRegistry, Database, MemoryDatabase, Symbol};
use nova_framework_dagger::DaggerAnalyzer;

fn load_fixture_sources(name: &str) -> Vec<(PathBuf, String)> {
    let root: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name);

    let mut out = Vec::new();
    collect_java_sources(&root, &mut out);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn collect_java_sources(dir: &Path, out: &mut Vec<(PathBuf, String)>) {
    for entry in std::fs::read_dir(dir).expect("read fixture dir") {
        let entry = entry.expect("read entry");
        let path = entry.path();
        if path.is_dir() {
            collect_java_sources(&path, out);
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("java") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("read java file");
        out.push((path, text));
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
        diags.iter().any(|d| d.code.as_ref() == "DAGGER_MISSING_BINDING"),
        "expected DAGGER_MISSING_BINDING diagnostic, got: {diags:#?}"
    );

    let diag = diags
        .into_iter()
        .find(|d| d.code.as_ref() == "DAGGER_MISSING_BINDING")
        .expect("missing binding diagnostic");

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
