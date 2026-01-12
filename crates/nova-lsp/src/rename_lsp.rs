use lsp_types::WorkspaceEdit as LspWorkspaceEdit;
use nova_refactor::{
    workspace_edit_to_lsp, workspace_edit_to_lsp_document_changes, RefactorDatabase,
    WorkspaceEdit as RefactorWorkspaceEdit,
};

/// Convert a [`nova_refactor::WorkspaceEdit`] returned by semantic rename into an LSP
/// [`WorkspaceEdit`].
///
/// We keep returning `changes` for simple edits (to preserve existing expectations in
/// tests/clients) but must use `documentChanges` whenever file operations (rename/create/delete)
/// are present.
pub(crate) fn rename_workspace_edit_to_lsp(
    db: &dyn RefactorDatabase,
    edit: &RefactorWorkspaceEdit,
) -> Result<LspWorkspaceEdit, String> {
    if edit.file_ops.is_empty() {
        workspace_edit_to_lsp(db, edit).map_err(|e| e.to_string())
    } else {
        workspace_edit_to_lsp_document_changes(db, edit).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_workspace_edit_with_file_ops_converts_to_document_changes() {
        let old_file = nova_refactor::FileId::new("file:///Old.java");
        let new_file = nova_refactor::FileId::new("file:///New.java");

        let db = nova_refactor::TextDatabase::new([(
            old_file.clone(),
            "class Old {}".to_string(),
        )]);

        let edit = nova_refactor::WorkspaceEdit {
            file_ops: vec![nova_refactor::FileOp::Rename {
                from: old_file.clone(),
                to: new_file.clone(),
            }],
            text_edits: vec![nova_refactor::WorkspaceTextEdit::replace(
                new_file,
                nova_refactor::WorkspaceTextRange::new(6, 9),
                "New",
            )],
        };

        let lsp_edit = rename_workspace_edit_to_lsp(&db, &edit).expect("convert workspace edit");

        assert!(lsp_edit.changes.is_none(), "expected changes to be None");
        assert!(
            matches!(
                lsp_edit.document_changes,
                Some(lsp_types::DocumentChanges::Operations(_))
            ),
            "expected documentChanges operations, got: {lsp_edit:?}"
        );
    }
}

