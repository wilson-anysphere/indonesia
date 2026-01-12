use nova_refactor::{
    apply_workspace_edit, move_method, move_static_member, FileId, MoveMemberError,
    MoveMethodParams, MoveStaticMemberParams, WorkspaceEdit,
};
use nova_test_utils::assert_fixture_transformed;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn apply_edit(files: &mut BTreeMap<PathBuf, String>, edit: &WorkspaceEdit) {
    let by_id: BTreeMap<FileId, String> = files
        .iter()
        .map(|(path, text)| (FileId::new(path.to_string_lossy().into_owned()), text.clone()))
        .collect();

    let updated = apply_workspace_edit(&by_id, edit).expect("workspace edit applies cleanly");
    *files = updated
        .into_iter()
        .map(|(file, text)| (PathBuf::from(file.0), text))
        .collect();
}

#[test]
fn move_static_method_updates_call_sites() {
    let before = Path::new("tests/fixtures/move_static_method/before");
    let after = Path::new("tests/fixtures/move_static_method/after");
    assert_fixture_transformed(before, after, |files| {
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
    let before = Path::new("tests/fixtures/move_instance_method/before");
    let after = Path::new("tests/fixtures/move_instance_method/after");
    assert_fixture_transformed(before, after, |files| {
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
    let before = Path::new("tests/fixtures/move_static_collision/before");
    let files = nova_test_utils::load_fixture_dir(before);

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
