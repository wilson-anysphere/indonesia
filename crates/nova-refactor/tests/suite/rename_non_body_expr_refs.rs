use std::collections::BTreeMap;

use nova_refactor::{apply_workspace_edit, rename, FileId, RefactorJavaDatabase, RenameParams};
use pretty_assertions::assert_eq;

#[test]
fn rename_updates_non_body_expression_references_for_fields() {
    let foo_file = FileId::new("Foo.java");
    let use_file = FileId::new("Use.java");

    let foo_src = r#"package p; public class Foo { public static final int CONST = 1; public static int A = CONST; @interface Anno { int value(); } @Anno(CONST) public static int B = Foo.CONST; enum E { X(CONST), Y(Foo.CONST); E(int x) {} } public int m(int i) { int y = CONST; switch(i) { case CONST: return 1; case Foo.CONST: return 2; default: return 0; } } }"#;
    let use_src = r#"package p; public class Use { int x = Foo.CONST; }"#;

    let db = RefactorJavaDatabase::new([
        (foo_file.clone(), foo_src.to_string()),
        (use_file.clone(), use_src.to_string()),
    ]);

    let offset = foo_src
        .find("CONST = 1")
        .expect("expected CONST declaration")
        + 1;
    let symbol = db.symbol_at(&foo_file, offset).expect("symbol at CONST");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "VALUE".into(),
        },
    )
    .unwrap();

    let files = BTreeMap::from([
        (foo_file.clone(), foo_src.to_string()),
        (use_file.clone(), use_src.to_string()),
    ]);
    let updated = apply_workspace_edit(&files, &edit).unwrap();

    let expected_foo = r#"package p; public class Foo { public static final int VALUE = 1; public static int A = VALUE; @interface Anno { int value(); } @Anno(VALUE) public static int B = Foo.VALUE; enum E { X(VALUE), Y(Foo.VALUE); E(int x) {} } public int m(int i) { int y = VALUE; switch(i) { case VALUE: return 1; case Foo.VALUE: return 2; default: return 0; } } }"#;
    let expected_use = r#"package p; public class Use { int x = Foo.VALUE; }"#;

    assert_eq!(updated.get(&foo_file).unwrap(), expected_foo);
    assert_eq!(updated.get(&use_file).unwrap(), expected_use);
}

