use nova_refactor::{
    apply_workspace_edit, move_package_workspace_edit, FileId, FileOp, MovePackageParams,
    WorkspaceEdit,
};
use pretty_assertions::assert_eq;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

fn apply_edit(
    files: &BTreeMap<PathBuf, String>,
    edit: &WorkspaceEdit,
) -> BTreeMap<PathBuf, String> {
    let by_id: BTreeMap<FileId, String> = files
        .iter()
        .map(|(path, text)| {
            (
                FileId::new(path.to_string_lossy().into_owned()),
                text.clone(),
            )
        })
        .collect();

    let updated = apply_workspace_edit(&by_id, edit).expect("workspace edit applies cleanly");
    updated
        .into_iter()
        .map(|(file, text)| (PathBuf::from(file.0), text))
        .collect()
}

#[test]
fn move_package_workspace_edit_renames_files_and_rewrites_references() {
    let mut files: BTreeMap<PathBuf, String> = BTreeMap::new();
    files.insert(
        PathBuf::from("src/main/java/com/old/A.java"),
        r#"package com.old;

public class A {}
"#
        .to_string(),
    );
    files.insert(
        PathBuf::from("src/main/java/com/old/sub/B.java"),
        r#"package com.old.sub;

import com.old.A;

public class B { A a; }
"#
        .to_string(),
    );
    files.insert(
        PathBuf::from("src/main/java/com/other/C.java"),
        r#"package com.other;

import com.old.A;

public class C {
    com.old.sub.B b;
    A a;
    String literal = "com.old.sub.B";
    // com.old.sub.B (comment)
}
"#
        .to_string(),
    );

    let edit = move_package_workspace_edit(
        &files,
        MovePackageParams {
            old_package: "com.old".into(),
            // Note: `com.new` would be rejected by `nova_build_model::validate_package_name` because
            // `new` is a reserved Java keyword, so we use a close-but-valid name here.
            new_package: "com.newpkg".into(),
        },
    )
    .expect("refactoring succeeds");

    // Ensure the canonical workspace edit expresses this refactoring as file renames.
    let renames: BTreeSet<(String, String)> = edit
        .file_ops
        .iter()
        .filter_map(|op| match op {
            FileOp::Rename { from, to } => Some((from.0.clone(), to.0.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        renames,
        BTreeSet::from([
            (
                "src/main/java/com/old/A.java".to_string(),
                "src/main/java/com/newpkg/A.java".to_string(),
            ),
            (
                "src/main/java/com/old/sub/B.java".to_string(),
                "src/main/java/com/newpkg/sub/B.java".to_string(),
            ),
        ])
    );

    // Ensure text edits target post-rename file ids (required for LSP rename-file + text edits).
    let edited_files: BTreeSet<&str> = edit.text_edits.iter().map(|e| e.file.0.as_str()).collect();
    assert_eq!(
        edited_files,
        BTreeSet::from([
            "src/main/java/com/newpkg/A.java",
            "src/main/java/com/newpkg/sub/B.java",
            "src/main/java/com/other/C.java",
        ])
    );

    // Apply the edit and assert the final workspace matches expectations.
    let applied = apply_edit(&files, &edit);

    let expected: BTreeMap<PathBuf, String> = BTreeMap::from([
        (
            PathBuf::from("src/main/java/com/newpkg/A.java"),
            r#"package com.newpkg;

public class A {}
"#
            .to_string(),
        ),
        (
            PathBuf::from("src/main/java/com/newpkg/sub/B.java"),
            r#"package com.newpkg.sub;

import com.newpkg.A;

public class B { A a; }
"#
            .to_string(),
        ),
        (
            PathBuf::from("src/main/java/com/other/C.java"),
            r#"package com.other;

import com.newpkg.A;

public class C {
    com.newpkg.sub.B b;
    A a;
    String literal = "com.old.sub.B";
    // com.old.sub.B (comment)
}
"#
            .to_string(),
        ),
    ]);

    assert_eq!(applied, expected);

    // Extra sanity: old locations are gone, and string/comment occurrences of `com.old` were not touched.
    assert!(!applied.contains_key(Path::new("src/main/java/com/old/A.java")));
    assert!(!applied.contains_key(Path::new("src/main/java/com/old/sub/B.java")));
    let c = &applied[Path::new("src/main/java/com/other/C.java")];
    assert!(c.contains(r#"String literal = "com.old.sub.B";"#));
    assert!(c.contains("// com.old.sub.B (comment)"));
}
