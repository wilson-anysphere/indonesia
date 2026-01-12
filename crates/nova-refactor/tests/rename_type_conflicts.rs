use nova_refactor::{rename, Conflict, FileId, RefactorJavaDatabase, RenameParams, SemanticRefactorError};

#[test]
fn rename_type_conflict_detected_across_workspace() {
    let foo_file = FileId::new("src/main/java/p/Foo.java");
    let bar_file = FileId::new("src/main/java/p/Bar.java");

    let foo_src = "package p; public class Foo {}";
    let bar_src = "package p; public class Bar {}";

    let db = RefactorJavaDatabase::new([
        (foo_file.clone(), foo_src.to_string()),
        (bar_file.clone(), bar_src.to_string()),
    ]);

    let offset = foo_src.find("Foo").expect("Foo name") + 1;
    let symbol = db.symbol_at(&foo_file, offset).expect("symbol at Foo");

    let err = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "Bar".into(),
        },
    )
    .unwrap_err();
    let SemanticRefactorError::Conflicts(conflicts) = err else {
        panic!("expected conflicts, got: {err:?}");
    };

    assert!(
        conflicts.iter().any(|c| matches!(
            c,
            Conflict::NameCollision { name, .. } if name == "Bar"
        )),
        "expected NameCollision conflict: {conflicts:?}"
    );
}

