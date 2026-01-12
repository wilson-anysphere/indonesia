use nova_refactor::{
    generate_preview, FileChangeKind, FileId, FileOp, TextDatabase, WorkspaceEdit,
    WorkspaceTextEdit,
};
use pretty_assertions::assert_eq;

#[test]
fn preview_surfaces_rename_file_ops_and_diff_for_destination() {
    let from = FileId::new("src/old.txt");
    let to = FileId::new("src/new.txt");

    let original = "hello\n".to_string();
    let db = TextDatabase::new([(from.clone(), original.clone())]);

    let edit = WorkspaceEdit {
        file_ops: vec![FileOp::Rename {
            from: from.clone(),
            to: to.clone(),
        }],
        text_edits: vec![WorkspaceTextEdit::insert(
            to.clone(),
            original.len(),
            "world\n",
        )],
    };

    let preview = generate_preview(&db, &edit).expect("preview generation succeeds");

    assert_eq!(
        preview.file_ops,
        vec![FileOp::Rename {
            from: from.clone(),
            to: to.clone()
        }]
    );

    let file_preview = preview
        .files
        .iter()
        .find(|f| f.file == to)
        .expect("expected preview for renamed destination file");

    assert_eq!(
        file_preview.change,
        FileChangeKind::Renamed {
            from: from.clone(),
            to: to.clone()
        }
    );
    assert!(
        file_preview.unified_diff.contains("--- a/src/old.txt"),
        "expected unified diff to reference old path; got:\n{}",
        file_preview.unified_diff
    );
    assert!(
        file_preview.unified_diff.contains("+++ b/src/new.txt"),
        "expected unified diff to reference new path; got:\n{}",
        file_preview.unified_diff
    );
    assert!(
        file_preview.unified_diff.contains("+world"),
        "expected unified diff to contain inserted text; got:\n{}",
        file_preview.unified_diff
    );

    assert!(
        !preview.files.iter().any(|f| f.file == from),
        "rename sources should not show up as 'deleted' file previews; got: {preview:?}"
    );
}
