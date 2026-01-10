use std::collections::HashMap;
use std::path::{Path, PathBuf};

use lsp_types::{
    ClientCapabilities, DocumentChangeOperation, DocumentChanges, OptionalVersionedTextDocumentIdentifier,
    ResourceOp, ResourceOperationKind, TextDocumentEdit, TextEdit, Uri, WorkspaceEdit,
};

use nova_refactor::RefactoringEdit;

pub fn client_supports_file_operations(capabilities: &ClientCapabilities) -> bool {
    let Some(workspace) = &capabilities.workspace else {
        return false;
    };
    let Some(edit) = &workspace.workspace_edit else {
        return false;
    };

    let supports_document_changes = edit.document_changes.unwrap_or(false);
    let supports_resource_ops = edit
        .resource_operations
        .as_ref()
        .map(|ops| !ops.is_empty())
        .unwrap_or(false);

    supports_document_changes && supports_resource_ops
}

fn can_rename(capabilities: &ClientCapabilities) -> bool {
    let Some(workspace) = &capabilities.workspace else {
        return false;
    };
    let Some(edit) = &workspace.workspace_edit else {
        return false;
    };
    edit.resource_operations
        .as_ref()
        .map(|ops| ops.contains(&ResourceOperationKind::Rename))
        .unwrap_or(false)
        && edit.document_changes.unwrap_or(false)
}

pub fn workspace_edit_from_refactor(
    root_uri: &Uri,
    original_files: &HashMap<PathBuf, String>,
    edit: &RefactoringEdit,
    capabilities: &ClientCapabilities,
) -> WorkspaceEdit {
    if can_rename(capabilities) {
        let mut ops: Vec<DocumentChangeOperation> = Vec::new();

        for mv in &edit.file_moves {
            let old_uri = join_uri(root_uri, &mv.old_path);
            let new_uri = join_uri(root_uri, &mv.new_path);

            ops.push(DocumentChangeOperation::Op(ResourceOp::Rename(lsp_types::RenameFile {
                old_uri,
                new_uri: new_uri.clone(),
                options: None,
                annotation_id: None,
            })));

            let old_contents = original_files
                .get(&mv.old_path)
                .map(String::as_str)
                .unwrap_or_default();
            ops.push(DocumentChangeOperation::Edit(TextDocumentEdit {
                text_document: OptionalVersionedTextDocumentIdentifier {
                    uri: new_uri,
                    version: None,
                },
                edits: vec![lsp_types::OneOf::Left(TextEdit {
                    range: full_document_range(old_contents),
                    new_text: mv.new_contents.clone(),
                })],
            }));
        }

        for fe in &edit.file_edits {
            let uri = join_uri(root_uri, &fe.path);
            let old_contents = original_files
                .get(&fe.path)
                .map(String::as_str)
                .unwrap_or_default();
            ops.push(DocumentChangeOperation::Edit(TextDocumentEdit {
                text_document: OptionalVersionedTextDocumentIdentifier { uri, version: None },
                edits: vec![lsp_types::OneOf::Left(TextEdit {
                    range: full_document_range(old_contents),
                    new_text: fe.new_contents.clone(),
                })],
            }));
        }

        WorkspaceEdit {
            changes: None,
            document_changes: Some(DocumentChanges::Operations(ops)),
            change_annotations: None,
        }
    } else {
        // Fallback: no file operations, so rewrite the original documents in place.
        let mut changes: HashMap<Uri, Vec<TextEdit>> = HashMap::new();

        for mv in &edit.file_moves {
            let uri = join_uri(root_uri, &mv.old_path);
            let old_contents = original_files
                .get(&mv.old_path)
                .map(String::as_str)
                .unwrap_or_default();
            changes.insert(
                uri,
                vec![TextEdit {
                    range: full_document_range(old_contents),
                    new_text: mv.new_contents.clone(),
                }],
            );
        }

        for fe in &edit.file_edits {
            let uri = join_uri(root_uri, &fe.path);
            let old_contents = original_files
                .get(&fe.path)
                .map(String::as_str)
                .unwrap_or_default();
            changes.insert(
                uri,
                vec![TextEdit {
                    range: full_document_range(old_contents),
                    new_text: fe.new_contents.clone(),
                }],
            );
        }

        WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        }
    }
}

fn join_uri(root: &Uri, path: &Path) -> Uri {
    let mut uri = root.as_str().to_string();
    if !uri.ends_with('/') {
        uri.push('/');
    }

    for (idx, component) in path.components().enumerate() {
        if idx > 0 {
            uri.push('/');
        }
        uri.push_str(&component.as_os_str().to_string_lossy());
    }

    uri.parse().expect("joined uri should be valid")
}

fn full_document_range(contents: &str) -> lsp_types::Range {
    let end = end_position(contents);
    lsp_types::Range {
        start: lsp_types::Position { line: 0, character: 0 },
        end,
    }
}

fn end_position(contents: &str) -> lsp_types::Position {
    let mut line: u32 = 0;
    let mut col_utf16: u32 = 0;
    for ch in contents.chars() {
        if ch == '\n' {
            line += 1;
            col_utf16 = 0;
            continue;
        }
        col_utf16 += ch.len_utf16() as u32;
    }
    lsp_types::Position {
        line,
        character: col_utf16,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use lsp_types::{WorkspaceClientCapabilities, WorkspaceEditClientCapabilities};

    #[test]
    fn workspace_edit_includes_rename_operation_when_supported() {
        let mut original = HashMap::new();
        original.insert(
            PathBuf::from("src/main/java/com/foo/A.java"),
            "package com.foo;\n\npublic class A {}\n".to_string(),
        );

        let edit = RefactoringEdit {
            file_moves: vec![nova_refactor::FileMove {
                old_path: PathBuf::from("src/main/java/com/foo/A.java"),
                new_path: PathBuf::from("src/main/java/com/bar/A.java"),
                new_contents: "package com.bar;\n\npublic class A {}\n".to_string(),
            }],
            file_edits: Vec::new(),
        };

        let caps = ClientCapabilities {
            workspace: Some(WorkspaceClientCapabilities {
                workspace_edit: Some(WorkspaceEditClientCapabilities {
                    document_changes: Some(true),
                    resource_operations: Some(vec![ResourceOperationKind::Rename]),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let root: Uri = "file:///workspace/".parse().unwrap();
        let ws_edit = workspace_edit_from_refactor(&root, &original, &edit, &caps);

        let Some(DocumentChanges::Operations(ops)) = ws_edit.document_changes else {
            panic!("expected document change operations");
        };
        assert!(ops.iter().any(|op| matches!(
            op,
            DocumentChangeOperation::Op(ResourceOp::Rename(_))
        )));
    }

    #[test]
    fn workspace_edit_falls_back_to_text_edits_without_file_ops_support() {
        let mut original = HashMap::new();
        original.insert(
            PathBuf::from("src/main/java/com/foo/A.java"),
            "package com.foo;\n\npublic class A {}\n".to_string(),
        );

        let edit = RefactoringEdit {
            file_moves: vec![nova_refactor::FileMove {
                old_path: PathBuf::from("src/main/java/com/foo/A.java"),
                new_path: PathBuf::from("src/main/java/com/bar/A.java"),
                new_contents: "package com.bar;\n\npublic class A {}\n".to_string(),
            }],
            file_edits: Vec::new(),
        };

        let caps = ClientCapabilities::default();

        let root: Uri = "file:///workspace/".parse().unwrap();
        let ws_edit = workspace_edit_from_refactor(&root, &original, &edit, &caps);

        assert!(ws_edit.document_changes.is_none());
        let changes = ws_edit.changes.expect("expected changes map");
        let uri = join_uri(&root, Path::new("src/main/java/com/foo/A.java"));
        assert!(changes.contains_key(&uri));
    }
}
