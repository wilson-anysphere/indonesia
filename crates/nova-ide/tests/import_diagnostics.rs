use std::path::PathBuf;

use nova_db::InMemoryFileStore;
use nova_ide::file_diagnostics;
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
