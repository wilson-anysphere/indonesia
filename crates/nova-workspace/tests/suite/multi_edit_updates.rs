use nova_core::{Position, Range};
use nova_db::{Database as _, NovaInputs as _};
use nova_syntax::TextEdit as SyntaxTextEdit;
use nova_vfs::{ContentChange, VfsPath};
use nova_workspace::Workspace;

fn apply_syntax_edit(source: &str, edit: &SyntaxTextEdit) -> String {
    let start = edit.range.start as usize;
    let end = edit.range.end as usize;
    assert!(
        start <= end && end <= source.len(),
        "edit range out of bounds"
    );
    assert!(
        source.is_char_boundary(start) && source.is_char_boundary(end),
        "edit range not aligned to UTF-8 boundaries"
    );
    let mut out = source.to_string();
    out.replace_range(start..end, &edit.replacement);
    out
}

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
    let initial =
        "class Main {\n  void m() {\n    int x = 1;\n    int y = 2;\n  }\n}\n".to_string();
    let file_id = workspace.open_document(vfs_path.clone(), initial.clone(), 1);

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

    // Ensure we used the incremental-update path even though VFS produced multiple edits.
    // `apply_file_text_edit` should have recorded a single synthetic edit + previous text snapshot.
    let snap_db = workspace
        .snapshot()
        .salsa_db()
        .expect("expected workspace snapshot to carry SalsaDatabase");
    snap_db.with_snapshot(|snap| {
        let prev = snap.file_prev_content(file_id);
        let last_edit = snap
            .file_last_edit(file_id)
            .expect("expected a synthetic last_edit for multi-edit update");
        let reconstructed = apply_syntax_edit(prev.as_str(), &last_edit);
        assert_eq!(prev.as_str(), initial.as_str());
        assert_eq!(reconstructed, snap.file_content(file_id).as_str());
    });

    workspace.close_document(&vfs_path);
    let snapshot = workspace.snapshot();

    assert_eq!(
        snapshot.file_content(file_id),
        "class Main {\n  void m() {\n    int x = 1; /*a*/\n    int y = 2; /*b*/\n  }\n}\n"
    );
}
