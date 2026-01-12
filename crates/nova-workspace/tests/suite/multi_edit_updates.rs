use nova_core::{Position, Range};
use nova_db::Database as _;
use nova_vfs::{ContentChange, VfsPath};
use nova_workspace::Workspace;

#[test]
fn multi_edit_change_batch_updates_salsa_file_content() {
    let workspace = Workspace::new_in_memory();

    // We want to validate the Salsa-backed `file_content`, not the open-document overlay. To do
    // that, we close the document and force the VFS `read_to_string` to fail by pointing at a
    // directory path.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Main.java");
    std::fs::create_dir_all(&path).unwrap();

    let vfs_path = VfsPath::local(path);
    let file_id = workspace.open_document(
        vfs_path.clone(),
        "class Main {\n  void m() {\n    int x = 1;\n    int y = 2;\n  }\n}\n".to_string(),
        1,
    );

    let edits = workspace
        .apply_changes(
            &vfs_path,
            2,
            &[
                ContentChange::replace(
                    Range::new(Position::new(2, 14), Position::new(2, 14)),
                    " /*a*/".to_string(),
                ),
                ContentChange::replace(
                    Range::new(Position::new(3, 14), Position::new(3, 14)),
                    " /*b*/".to_string(),
                ),
            ],
        )
        .unwrap();
    assert_eq!(edits.len(), 2);

    workspace.close_document(&vfs_path);
    let snapshot = workspace.snapshot();

    assert_eq!(
        snapshot.file_content(file_id),
        "class Main {\n  void m() {\n    int x = 1; /*a*/\n    int y = 2; /*b*/\n  }\n}\n"
    );
}

