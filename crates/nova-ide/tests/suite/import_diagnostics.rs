use std::path::PathBuf;

use nova_db::InMemoryFileStore;
use nova_ide::{core_file_diagnostics, file_diagnostics};
use nova_scheduler::CancellationToken;
use nova_types::Severity;

#[test]
fn file_diagnostics_includes_unresolved_import() {
    let mut db = InMemoryFileStore::new();
    let path = PathBuf::from("/test.java");
    let file = db.file_id_for_path(&path);
    db.set_file_text(
        file,
        r#"
import foo.Bar;
class A {}
"#
        .to_string(),
    );

    let diags = file_diagnostics(&db, file);
    assert!(
        diags.iter().any(|d| {
            d.code == "unresolved-import"
                && d.severity == Severity::Error
                && d.message.contains("foo.Bar")
        }),
        "expected unresolved-import diagnostic; got {diags:#?}"
    );
}

#[test]
fn core_file_diagnostics_does_not_duplicate_import_diagnostics() {
    let mut db = InMemoryFileStore::new();
    let path = PathBuf::from("/test.java");
    let file = db.file_id_for_path(&path);
    db.set_file_text(
        file,
        r#"
import foo.Bar;
class A {}
"#
        .to_string(),
    );

    let cancel = CancellationToken::new();
    let diags = core_file_diagnostics(&db, file, &cancel);
    let unresolved_imports = diags
        .iter()
        .filter(|d| d.code == "unresolved-import")
        .count();

    assert_eq!(
        unresolved_imports, 1,
        "expected exactly one unresolved-import diagnostic; got: {diags:#?}"
    );
}
