use nova_refactor::{
    apply_workspace_edit, move_package_workspace_edit,
    workspace_edit_to_lsp_document_changes_with_uri_mapper, FileId, FileOp, MovePackageParams,
    TextDatabase, WorkspaceEdit,
};
use pretty_assertions::assert_eq;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use lsp_types::{DocumentChangeOperation, DocumentChanges, ResourceOp, Uri};

const OLD_PACKAGE: &str = "com.old";
// Note: `com.new` would be rejected by `nova_build_model::validate_package_name` because `new` is
// a reserved Java keyword, so we use a close-but-valid name here.
const NEW_PACKAGE: &str = "com.newpkg";

fn fixture_files() -> BTreeMap<PathBuf, String> {
    BTreeMap::from([
        (
            PathBuf::from("src/main/java/com/old/A.java"),
            r#"package com.old;

public class A {}
"#
            .to_string(),
        ),
        (
            PathBuf::from("src/main/java/com/old/sub/B.java"),
            r#"package com.old.sub;

import com.old.A;

public class B { A a; }
"#
            .to_string(),
        ),
        (
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
        ),
    ])
}

fn expected_files() -> BTreeMap<PathBuf, String> {
    BTreeMap::from([
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
    ])
}

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
    let files = fixture_files();

    let edit = move_package_workspace_edit(
        &files,
        MovePackageParams {
            old_package: OLD_PACKAGE.into(),
            new_package: NEW_PACKAGE.into(),
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

    assert_eq!(applied, expected_files());

    // Extra sanity: old locations are gone, and string/comment occurrences of `com.old` were not touched.
    assert!(!applied.contains_key(Path::new("src/main/java/com/old/A.java")));
    assert!(!applied.contains_key(Path::new("src/main/java/com/old/sub/B.java")));
    let c = &applied[Path::new("src/main/java/com/other/C.java")];
    assert!(c.contains(r#"String literal = "com.old.sub.B";"#));
    assert!(c.contains("// com.old.sub.B (comment)"));
}

#[test]
fn move_package_workspace_edit_lsp_document_changes_include_renames_and_new_file_edits() {
    let files = fixture_files();
    let expected = expected_files();

    let edit = move_package_workspace_edit(
        &files,
        MovePackageParams {
            old_package: OLD_PACKAGE.into(),
            new_package: NEW_PACKAGE.into(),
        },
    )
    .expect("refactoring succeeds");

    let db = TextDatabase::new(files.iter().map(|(path, text)| {
        (
            FileId::new(path.to_string_lossy().into_owned()),
            text.to_string(),
        )
    }));

    let root: Uri = "file:///workspace/".parse().unwrap();
    let lsp = workspace_edit_to_lsp_document_changes_with_uri_mapper(&db, &edit, |f| {
        let uri: Uri = format!("{}{}", root.as_str(), f.0).parse().unwrap();
        Ok(uri)
    })
    .expect("LSP conversion succeeds");

    let Some(DocumentChanges::Operations(ops)) = lsp.document_changes else {
        panic!("expected document changes operations; got: {lsp:?}");
    };

    let rename_ops: BTreeSet<(String, String)> = ops
        .iter()
        .filter_map(|op| match op {
            DocumentChangeOperation::Op(ResourceOp::Rename(rename)) => {
                Some((rename.old_uri.to_string(), rename.new_uri.to_string()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        rename_ops,
        BTreeSet::from([
            (
                "file:///workspace/src/main/java/com/old/A.java".to_string(),
                "file:///workspace/src/main/java/com/newpkg/A.java".to_string(),
            ),
            (
                "file:///workspace/src/main/java/com/old/sub/B.java".to_string(),
                "file:///workspace/src/main/java/com/newpkg/sub/B.java".to_string(),
            ),
        ])
    );

    // Rename file ops must come before text edits for the destination URIs.
    let first_edit_idx = ops
        .iter()
        .position(|op| matches!(op, DocumentChangeOperation::Edit(_)))
        .expect("expected at least one TextDocumentEdit");
    assert!(
        ops[..first_edit_idx]
            .iter()
            .all(|op| matches!(op, DocumentChangeOperation::Op(_))),
        "expected all ops before first edit to be ResourceOp; got: {ops:?}"
    );

    // Extract the final replacement text per edited document and compare to the expected workspace.
    let mut edits_by_uri: BTreeMap<String, String> = BTreeMap::new();
    for op in &ops {
        let DocumentChangeOperation::Edit(text_doc_edit) = op else {
            continue;
        };
        assert_eq!(
            text_doc_edit.edits.len(),
            1,
            "expected one full-document replacement per file; got: {text_doc_edit:?}"
        );
        let edit = match &text_doc_edit.edits[0] {
            lsp_types::OneOf::Left(e) => e,
            lsp_types::OneOf::Right(e) => &e.text_edit,
        };
        edits_by_uri.insert(
            text_doc_edit.text_document.uri.to_string(),
            edit.new_text.clone(),
        );
    }

    let expected_by_uri: BTreeMap<String, String> = expected
        .iter()
        .map(|(path, text)| {
            (
                format!("{}{}", root.as_str(), path.to_string_lossy()),
                text.to_string(),
            )
        })
        .collect();

    assert_eq!(edits_by_uri, expected_by_uri);
}
