use std::collections::BTreeMap;

use nova_refactor::{
    apply_workspace_edit, rename, FileId, FileOp, RefactorJavaDatabase, RenameParams,
};

#[test]
fn rename_package_moves_files_and_updates_references() {
    let a_file = FileId::new("src/main/java/com/foo/A.java");
    let b_file = FileId::new("src/main/java/com/foo/sub/B.java");
    let c_file = FileId::new("src/main/java/com/other/C.java");

    let a_src = r#"package com.foo;
public class A {}
"#;

    let b_src = r#"package com.foo.sub;
import com.foo.A;
public class B { A a; }
"#;

    let c_src = r#"package com.other;
import com.foo.sub.B;
public class C { B b; com.foo.sub.B qb; }
"#;

    let files: BTreeMap<FileId, String> = [
        (a_file.clone(), a_src.to_string()),
        (b_file.clone(), b_src.to_string()),
        (c_file.clone(), c_src.to_string()),
    ]
    .into_iter()
    .collect();

    let db = RefactorJavaDatabase::new(files.clone());

    let offset = a_src.find("com.foo").expect("package name present") + 1;
    let symbol = db
        .symbol_at(&a_file, offset)
        .expect("symbol at package name");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "com.bar".into(),
        },
    )
    .unwrap();

    assert!(
        edit.file_ops.contains(&FileOp::Rename {
            from: a_file.clone(),
            to: FileId::new("src/main/java/com/bar/A.java"),
        }),
        "expected A.java rename op, got: {:?}",
        edit.file_ops
    );
    assert!(
        edit.file_ops.contains(&FileOp::Rename {
            from: b_file.clone(),
            to: FileId::new("src/main/java/com/bar/sub/B.java"),
        }),
        "expected B.java rename op, got: {:?}",
        edit.file_ops
    );

    let applied = apply_workspace_edit(&files, &edit).expect("workspace edit applies cleanly");

    assert!(applied.contains_key(&FileId::new("src/main/java/com/bar/A.java")));
    assert!(applied.contains_key(&FileId::new("src/main/java/com/bar/sub/B.java")));
    assert!(!applied.contains_key(&a_file));
    assert!(!applied.contains_key(&b_file));

    let a = &applied[&FileId::new("src/main/java/com/bar/A.java")];
    assert!(a.contains("package com.bar;"));

    let b = &applied[&FileId::new("src/main/java/com/bar/sub/B.java")];
    assert!(b.contains("package com.bar.sub;"));
    assert!(b.contains("import com.bar.A;"));

    let c = &applied[&c_file];
    assert!(c.contains("import com.bar.sub.B;"));
    assert!(c.contains("com.bar.sub.B qb;"));
    assert!(!c.contains("com.foo.sub.B"));
}

#[test]
fn rename_package_moves_files_and_updates_references_for_file_uri_ids() {
    let a_file = FileId::new("file:///workspace/src/main/java/com/foo/A.java");
    let b_file = FileId::new("file:///workspace/src/main/java/com/foo/sub/B.java");
    let c_file = FileId::new("file:///workspace/src/main/java/com/other/C.java");

    let a_src = r#"package com.foo;
public class A {}
"#;

    let b_src = r#"package com.foo.sub;
import com.foo.A;
public class B { A a; }
"#;

    let c_src = r#"package com.other;
import com.foo.sub.B;
public class C { B b; com.foo.sub.B qb; }
"#;

    let files: BTreeMap<FileId, String> = [
        (a_file.clone(), a_src.to_string()),
        (b_file.clone(), b_src.to_string()),
        (c_file.clone(), c_src.to_string()),
    ]
    .into_iter()
    .collect();

    let db = RefactorJavaDatabase::new(files.clone());

    let offset = a_src.find("com.foo").expect("package name present") + 1;
    let symbol = db
        .symbol_at(&a_file, offset)
        .expect("symbol at package name");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "com.bar".into(),
        },
    )
    .unwrap();

    assert!(
        edit.file_ops.contains(&FileOp::Rename {
            from: a_file.clone(),
            to: FileId::new("file:///workspace/src/main/java/com/bar/A.java"),
        }),
        "expected A.java rename op, got: {:?}",
        edit.file_ops
    );
    assert!(
        edit.file_ops.contains(&FileOp::Rename {
            from: b_file.clone(),
            to: FileId::new("file:///workspace/src/main/java/com/bar/sub/B.java"),
        }),
        "expected B.java rename op, got: {:?}",
        edit.file_ops
    );

    let applied = apply_workspace_edit(&files, &edit).expect("workspace edit applies cleanly");

    assert!(applied.contains_key(&FileId::new(
        "file:///workspace/src/main/java/com/bar/A.java"
    )));
    assert!(applied.contains_key(&FileId::new(
        "file:///workspace/src/main/java/com/bar/sub/B.java"
    )));
    assert!(!applied.contains_key(&a_file));
    assert!(!applied.contains_key(&b_file));

    let a = &applied[&FileId::new("file:///workspace/src/main/java/com/bar/A.java")];
    assert!(a.contains("package com.bar;"));

    let b = &applied[&FileId::new("file:///workspace/src/main/java/com/bar/sub/B.java")];
    assert!(b.contains("package com.bar.sub;"));
    assert!(b.contains("import com.bar.A;"));

    let c = &applied[&c_file];
    assert!(c.contains("import com.bar.sub.B;"));
    assert!(c.contains("com.bar.sub.B qb;"));
    assert!(!c.contains("com.foo.sub.B"));
}
