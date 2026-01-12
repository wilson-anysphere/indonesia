use std::path::{Path, PathBuf};

use nova_db::{Database as _, InMemoryFileStore};
use nova_ide::{file_diagnostics, goto_definition};

use crate::framework_harness::offset_to_position as offset_to_lsp_position;

fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../nova-framework-dagger/tests/fixtures")
}

fn load_fixture(name: &str) -> (InMemoryFileStore, Vec<PathBuf>) {
    let root = fixtures_root().join(name);
    let mut paths = Vec::new();
    collect_java_files(&root, &mut paths);
    paths.sort();

    let mut db = InMemoryFileStore::new();
    for path in &paths {
        let text = std::fs::read_to_string(path).expect("read java fixture file");
        let id = db.file_id_for_path(path);
        db.set_file_text(id, text);
    }

    (db, paths)
}

fn collect_java_files(dir: &Path, out: &mut Vec<PathBuf>) {
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
        out.push(path);
    }
}

fn slice_lsp_range(text: &str, range: lsp_types::Range) -> String {
    let index = nova_core::LineIndex::new(text);
    let start = index
        .offset_of_position(
            text,
            nova_core::Position::new(range.start.line, range.start.character),
        )
        .expect("start position in bounds");
    let end = index
        .offset_of_position(
            text,
            nova_core::Position::new(range.end.line, range.end.character),
        )
        .expect("end position in bounds");
    text[u32::from(start) as usize..u32::from(end) as usize].to_string()
}

#[test]
fn missing_binding_diagnostic_is_reported_with_correct_span() {
    let (db, paths) = load_fixture("missing_binding");
    let foo_path = paths
        .iter()
        .find(|p| p.ends_with("Foo.java"))
        .expect("Foo.java path");
    let foo_file = db.file_id(foo_path).expect("Foo.java file id");
    let foo_text = db.file_content(foo_file);

    let diags = file_diagnostics(&db, foo_file);
    let diag = diags
        .iter()
        .find(|d| d.code == "DAGGER_MISSING_BINDING")
        .expect("missing binding diagnostic from Dagger");

    let span = diag.span.expect("Dagger diagnostic should have a span");
    assert_eq!(&foo_text[span.start..span.end], "Bar");
}

#[test]
fn goto_definition_from_injection_jumps_to_provider() {
    let (db, paths) = load_fixture("navigation_multiline");
    let consumer_path = paths
        .iter()
        .find(|p| p.ends_with("Consumer.java"))
        .expect("Consumer.java path");
    let module_path = paths
        .iter()
        .find(|p| p.ends_with("FooModule.java"))
        .expect("FooModule.java path");

    let consumer_file = db.file_id(consumer_path).expect("Consumer.java file id");
    let consumer_text = db.file_content(consumer_file);
    let foo_offset = consumer_text.find("Foo").expect("Foo injection token");
    let pos = offset_to_lsp_position(consumer_text, foo_offset);

    let loc = goto_definition(&db, consumer_file, pos).expect("expected provider definition");
    assert!(
        loc.uri.as_str().contains("FooModule.java"),
        "expected goto-definition URI to point to FooModule.java, got {:?}",
        loc.uri
    );

    let module_file = db.file_id(module_path).expect("FooModule.java file id");
    let module_text = db.file_content(module_file);
    assert_eq!(slice_lsp_range(module_text, loc.range), "provideFoo");
}
