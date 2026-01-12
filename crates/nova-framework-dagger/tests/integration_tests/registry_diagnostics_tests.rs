use std::path::{Path, PathBuf};

use nova_framework::{AnalyzerRegistry, Database, MemoryDatabase};
use nova_framework_dagger::DaggerAnalyzer;
use nova_types::Severity;

fn load_fixture_into_db(
    db: &mut MemoryDatabase,
    project: nova_core::ProjectId,
    name: &str,
) -> Vec<(PathBuf, nova_core::FileId)> {
    let root: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name);

    let mut out = Vec::new();
    collect_java_files(&root, &mut out);

    out.into_iter()
        .map(|(path, text)| {
            let file_id = db.add_file_with_path_and_text(project, &path, text);
            (path, file_id)
        })
        .collect()
}

fn collect_java_files(dir: &Path, out: &mut Vec<(PathBuf, String)>) {
    for entry in std::fs::read_dir(dir).expect("read fixture dir") {
        let entry = entry.expect("read entry");
        let path = entry.path();
        if path.is_dir() {
            collect_java_files(&path, out);
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
fn analyzer_registry_surfaces_missing_binding_diagnostic() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_dependency(project, "com.google.dagger", "dagger");

    let files = load_fixture_into_db(&mut db, project, "missing_binding");
    let foo_file = files
        .iter()
        .find(|(path, _)| path.ends_with("Foo.java"))
        .expect("Foo.java")
        .1;

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(DaggerAnalyzer::default()));

    let diagnostics = registry.framework_diagnostics(&db, foo_file);
    let diag = diagnostics
        .iter()
        .find(|d| d.code.as_ref() == "DAGGER_MISSING_BINDING")
        .expect("missing binding diagnostic from registry");

    assert_eq!(diag.severity, Severity::Error);
    assert!(diag.message.contains("Missing binding"));
    assert!(diag.message.contains("Bar"));

    let span = diag.span.expect("expected span for diagnostic");
    let text = db.file_text(foo_file).expect("file text");
    assert_eq!(&text[span.start..span.end], "Bar");
}

#[test]
fn analyzer_registry_surfaces_duplicate_binding_diagnostic() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_dependency(project, "com.google.dagger", "dagger");

    let files = load_fixture_into_db(&mut db, project, "duplicate_binding");
    let consumer_file = files
        .iter()
        .find(|(path, _)| path.ends_with("Consumer.java"))
        .expect("Consumer.java")
        .1;

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(DaggerAnalyzer::default()));

    let diagnostics = registry.framework_diagnostics(&db, consumer_file);
    let diag = diagnostics
        .iter()
        .find(|d| d.code.as_ref() == "DAGGER_DUPLICATE_BINDING")
        .expect("duplicate binding diagnostic from registry");

    assert_eq!(diag.severity, Severity::Error);
    assert!(diag.message.contains("Duplicate bindings"));
    assert!(diag.message.contains("Foo"));

    let span = diag.span.expect("expected span for diagnostic");
    let text = db.file_text(consumer_file).expect("file text");
    assert_eq!(&text[span.start..span.end], "Foo");
}
