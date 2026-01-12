use std::sync::atomic::Ordering;

use nova_db::salsa::NovaHir;
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
    db.set_file_text(file, source);

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
