use std::sync::atomic::Ordering;
use std::sync::Arc;

use nova_db::salsa::{NovaHir, NovaInputs};
use nova_db::{FileId, SalsaRootDatabase};

#[test]
fn java_parse_query_does_not_reparse_text() {
    let source = r#"
class Foo {
    int x;
}
"#;

    nova_syntax::java::PARSE_TEXT_CALLS.store(0, Ordering::Relaxed);

    let mut db = SalsaRootDatabase::default();
    let file = FileId::from_raw(0);
    db.set_file_exists(file, true);
    db.set_file_content(file, Arc::new(source.to_string()));

    let snap = db.snapshot();
    let parse = snap.java_parse(file);

    // `java_parse` should lower from `db.parse_java(file)` and avoid calling the
    // string-based `nova_syntax::java::parse(text)` entrypoint (which reparses).
    assert_eq!(
        nova_syntax::java::PARSE_TEXT_CALLS.load(Ordering::Relaxed),
        0
    );

    let unit = parse.compilation_unit();
    assert_eq!(unit.types.len(), 1);
    assert_eq!(unit.types[0].name(), "Foo");
}

