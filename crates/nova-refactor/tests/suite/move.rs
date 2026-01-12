use nova_refactor::{
    apply_workspace_edit, generate_preview, move_class_workspace_edit, move_method,
    move_static_member, FileId, MoveClassParams, MoveMemberError, MoveMethodParams,
    MoveStaticMemberParams, TextDatabase, WorkspaceEdit,
};
use nova_test_utils::assert_fixture_transformed;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn fixture_dir(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(rel)
}

fn apply_edit(files: &mut BTreeMap<PathBuf, String>, edit: &WorkspaceEdit) {
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
    *files = updated
        .into_iter()
        .map(|(file, text)| (PathBuf::from(file.0), text))
        .collect();
}

#[test]
fn move_static_method_updates_call_sites() {
    let before = fixture_dir("tests/fixtures/move_static_method/before");
    let after = fixture_dir("tests/fixtures/move_static_method/after");
    assert_fixture_transformed(&before, &after, |files| {
        let edit = move_static_member(
            &*files,
            MoveStaticMemberParams {
                from_class: "A".into(),
                member_name: "add".into(),
                to_class: "B".into(),
            },
        )
        .expect("refactoring succeeds");
        apply_edit(files, &edit);
    });
}

#[test]
fn move_instance_method_adds_receiver_param_and_updates_calls() {
    let before = fixture_dir("tests/fixtures/move_instance_method/before");
    let after = fixture_dir("tests/fixtures/move_instance_method/after");
    assert_fixture_transformed(&before, &after, |files| {
        let edit = move_method(
            &*files,
            MoveMethodParams {
                from_class: "A".into(),
                method_name: "compute".into(),
                to_class: "B".into(),
            },
        )
        .expect("refactoring succeeds");
        apply_edit(files, &edit);
    });
}

#[test]
fn move_static_member_detects_collision() {
    let before = fixture_dir("tests/fixtures/move_static_collision/before");
    let files = nova_test_utils::load_fixture_dir(&before);

    let err = move_static_member(
        &files,
        MoveStaticMemberParams {
            from_class: "A".into(),
            member_name: "add".into(),
            to_class: "B".into(),
        },
    )
    .unwrap_err();

    assert_eq!(
        err,
        MoveMemberError::NameCollision {
            class: "B".into(),
            member: "add".into()
        }
    );
}

#[test]
fn move_class_workspace_edit_normalizes_and_generates_preview() {
    let mut files: BTreeMap<PathBuf, String> = BTreeMap::new();
    files.insert(
        PathBuf::from("src/main/java/com/foo/A.java"),
        "package com.foo;\n\npublic class A {}\n".to_string(),
    );
    files.insert(
        PathBuf::from("src/main/java/com/other/C.java"),
        "package com.other;\n\nimport com.foo.A;\n\npublic class C { A a; }\n".to_string(),
    );

    let edit = move_class_workspace_edit(
        &files,
        MoveClassParams {
            source_path: PathBuf::from("src/main/java/com/foo/A.java"),
            class_name: "A".into(),
            target_package: "com.bar".into(),
        },
    )
    .expect("refactoring succeeds");

    let mut normalized = edit.clone();
    normalized
        .normalize()
        .expect("workspace edit should normalize");

    let db = TextDatabase::new(files.iter().map(|(path, text)| {
        (
            FileId::new(path.to_string_lossy().into_owned()),
            text.clone(),
        )
    }));
    let preview = generate_preview(&db, &edit).expect("preview generation succeeds");

    assert!(
        preview
            .files
            .iter()
            .any(|f| f.file.0 == "src/main/java/com/bar/A.java"),
        "preview should include moved file; got: {preview:?}"
    );
}
