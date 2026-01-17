use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use lsp_types::{ClientCapabilities, ResourceOperationKind, Uri, WorkspaceEdit};

use nova_refactor::{
    apply_workspace_edit, workspace_edit_to_lsp_document_changes_with_uri_mapper,
    workspace_edit_to_lsp_with_uri_mapper, FileId as RefactorFileId, FileOp as RefactorFileOp,
    RefactorDatabase, TextDatabase, WorkspaceEdit as RefactorWorkspaceEdit, WorkspaceTextEdit,
};

pub fn client_supports_file_operations(capabilities: &ClientCapabilities) -> bool {
    can_rename(capabilities) || (can_create(capabilities) && can_delete(capabilities))
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
        .is_some_and(|ops| ops.contains(&ResourceOperationKind::Rename))
        && edit.document_changes.unwrap_or(false)
}

fn can_create(capabilities: &ClientCapabilities) -> bool {
    let Some(workspace) = &capabilities.workspace else {
        return false;
    };
    let Some(edit) = &workspace.workspace_edit else {
        return false;
    };
    edit.resource_operations
        .as_ref()
        .is_some_and(|ops| ops.contains(&ResourceOperationKind::Create))
        && edit.document_changes.unwrap_or(false)
}

fn can_delete(capabilities: &ClientCapabilities) -> bool {
    let Some(workspace) = &capabilities.workspace else {
        return false;
    };
    let Some(edit) = &workspace.workspace_edit else {
        return false;
    };
    edit.resource_operations
        .as_ref()
        .is_some_and(|ops| ops.contains(&ResourceOperationKind::Delete))
        && edit.document_changes.unwrap_or(false)
}

/// Convert a canonical [`nova_refactor::WorkspaceEdit`] into an LSP [`WorkspaceEdit`], selecting
/// the best representation for the provided client capabilities.
///
/// - If the client supports `documentChanges` + `Rename`, we emit `RenameFile`.
/// - If the client supports `documentChanges` + `Create`+`Delete` (but not `Rename`), we rewrite
///   `Rename` file ops into `Create`+`Delete` and preserve post-rename text edits.
/// - Otherwise, we fall back to rewriting the original documents in-place via the `changes` map
///   (matching the behavior of Nova's legacy move refactorings).
pub fn workspace_edit_from_refactor_workspace_edit(
    root_uri: &Uri,
    db: &dyn RefactorDatabase,
    edit: &RefactorWorkspaceEdit,
    capabilities: &ClientCapabilities,
) -> WorkspaceEdit {
    if can_rename(capabilities) {
        match workspace_edit_to_lsp_document_changes_with_uri_mapper(db, edit, |file| {
            Ok(join_uri(root_uri, Path::new(&file.0)))
        }) {
            Ok(edit) => return edit,
            Err(err) => {
                tracing::debug!(
                    target = "nova.lsp",
                    file_ops = edit.file_ops.len(),
                    text_edits = edit.text_edits.len(),
                    err = ?err,
                    "failed to convert workspace edit with Rename support; falling back"
                );
            }
        }
    }

    if can_create(capabilities) && can_delete(capabilities) {
        if let Some(rewritten) = rewrite_renames_as_create_delete(db, edit) {
            match workspace_edit_to_lsp_document_changes_with_uri_mapper(db, &rewritten, |file| {
                Ok(join_uri(root_uri, Path::new(&file.0)))
            }) {
                Ok(edit) => return edit,
                Err(err) => {
                    tracing::debug!(
                        target = "nova.lsp",
                        file_ops = rewritten.file_ops.len(),
                        text_edits = rewritten.text_edits.len(),
                        err = ?err,
                        "failed to convert rewritten workspace edit with Create/Delete support; falling back"
                    );
                }
            }
        }
    }

    match try_workspace_edit_without_file_ops_support(root_uri, db, edit) {
        Some(edit) => edit,
        None => {
            tracing::debug!(
                target = "nova.lsp",
                file_ops = edit.file_ops.len(),
                text_edits = edit.text_edits.len(),
                "falling back to empty workspace edit (client lacks file ops support)"
            );
            WorkspaceEdit::default()
        }
    }
}

pub fn workspace_edit_from_refactor(
    root_uri: &Uri,
    original_files: &HashMap<PathBuf, String>,
    edit: &RefactorWorkspaceEdit,
    capabilities: &ClientCapabilities,
) -> WorkspaceEdit {
    let db = TextDatabase::new(original_files.iter().map(|(path, text)| {
        (
            RefactorFileId::new(path.to_string_lossy().into_owned()),
            text.clone(),
        )
    }));

    workspace_edit_from_refactor_workspace_edit(root_uri, &db, edit, capabilities)
}

fn rewrite_renames_as_create_delete(
    db: &dyn RefactorDatabase,
    edit: &RefactorWorkspaceEdit,
) -> Option<RefactorWorkspaceEdit> {
    let mut canonical = RefactorWorkspaceEdit {
        file_ops: Vec::new(),
        text_edits: edit.text_edits.clone(),
    };

    for op in &edit.file_ops {
        match op {
            RefactorFileOp::Rename { from, to } => {
                let from_contents = match db.file_text(from) {
                    Some(text) => text.to_string(),
                    None => {
                        tracing::debug!(
                            target = "nova.lsp",
                            from = %from.0,
                            to = %to.0,
                            "missing file contents while rewriting rename as create+delete"
                        );
                        return None;
                    }
                };
                canonical.file_ops.push(RefactorFileOp::Create {
                    file: to.clone(),
                    contents: from_contents,
                });
                canonical
                    .file_ops
                    .push(RefactorFileOp::Delete { file: from.clone() });
            }
            RefactorFileOp::Create { file, contents } => {
                canonical.file_ops.push(RefactorFileOp::Create {
                    file: file.clone(),
                    contents: contents.clone(),
                })
            }
            RefactorFileOp::Delete { file } => canonical
                .file_ops
                .push(RefactorFileOp::Delete { file: file.clone() }),
        }
    }

    if let Err(err) = canonical.normalize() {
        tracing::debug!(
            target = "nova.lsp",
            file_ops = canonical.file_ops.len(),
            text_edits = canonical.text_edits.len(),
            err = ?err,
            "failed to normalize rewritten workspace edit"
        );
        return None;
    }
    Some(canonical)
}

fn try_workspace_edit_without_file_ops_support(
    root_uri: &Uri,
    db: &dyn RefactorDatabase,
    edit: &RefactorWorkspaceEdit,
) -> Option<WorkspaceEdit> {
    // Build a minimal pre-edit snapshot containing the files needed for this edit.
    let mut original_by_id: BTreeMap<RefactorFileId, String> = BTreeMap::new();

    for op in &edit.file_ops {
        match op {
            RefactorFileOp::Rename { from, to } => {
                let text = match db.file_text(from) {
                    Some(text) => text,
                    None => {
                        tracing::debug!(
                            target = "nova.lsp",
                            from = %from.0,
                            to = %to.0,
                            "missing file contents for rename source while rewriting workspace edit without file ops support"
                        );
                        return None;
                    }
                };
                original_by_id.insert(from.clone(), text.to_string());
                if let Some(text) = db.file_text(to) {
                    original_by_id.insert(to.clone(), text.to_string());
                }
            }
            RefactorFileOp::Delete { file } => {
                let text = match db.file_text(file) {
                    Some(text) => text,
                    None => {
                        tracing::debug!(
                            target = "nova.lsp",
                            file = %file.0,
                            "missing file contents for delete target while rewriting workspace edit without file ops support"
                        );
                        return None;
                    }
                };
                original_by_id.insert(file.clone(), text.to_string());
            }
            RefactorFileOp::Create { file, .. } => {
                // Include existing content to surface create conflicts.
                if let Some(text) = db.file_text(file) {
                    original_by_id.insert(file.clone(), text.to_string());
                }
            }
        }
    }

    for e in &edit.text_edits {
        if original_by_id.contains_key(&e.file) {
            continue;
        }
        if let Some(text) = db.file_text(&e.file) {
            original_by_id.insert(e.file.clone(), text.to_string());
        }
    }

    let applied = match apply_workspace_edit(&original_by_id, edit) {
        Ok(applied) => applied,
        Err(err) => {
            tracing::debug!(
                target = "nova.lsp",
                file_ops = edit.file_ops.len(),
                text_edits = edit.text_edits.len(),
                err = ?err,
                "failed to apply refactor workspace edit while rewriting rename operations"
            );
            return None;
        }
    };

    let mut rewritten_sources: HashSet<RefactorFileId> = HashSet::new();
    let mut rename_dests: HashSet<RefactorFileId> = HashSet::new();

    let mut canonical = RefactorWorkspaceEdit {
        file_ops: Vec::new(),
        text_edits: Vec::new(),
    };

    for op in &edit.file_ops {
        let RefactorFileOp::Rename { from, to } = op else {
            continue;
        };

        let old_contents = match original_by_id.get(from) {
            Some(text) => text,
            None => {
                tracing::debug!(
                    target = "nova.lsp",
                    from = %from.0,
                    to = %to.0,
                    "missing rename source contents while rewriting workspace edit without file ops support"
                );
                return None;
            }
        };
        let new_contents = match applied.get(to) {
            Some(text) => text,
            None => {
                tracing::debug!(
                    target = "nova.lsp",
                    from = %from.0,
                    to = %to.0,
                    "missing rename destination contents while rewriting workspace edit without file ops support"
                );
                return None;
            }
        };

        rewritten_sources.insert(from.clone());
        rename_dests.insert(to.clone());

        canonical.text_edits.push(WorkspaceTextEdit::replace(
            from.clone(),
            nova_refactor::TextRange::new(0, old_contents.len()),
            new_contents.clone(),
        ));
    }

    for e in &edit.text_edits {
        if rewritten_sources.contains(&e.file) || rename_dests.contains(&e.file) {
            continue;
        }
        if !original_by_id.contains_key(&e.file) {
            continue;
        }
        canonical.text_edits.push(e.clone());
    }

    if let Err(err) = canonical.normalize() {
        tracing::debug!(
            target = "nova.lsp",
            file_ops = canonical.file_ops.len(),
            text_edits = canonical.text_edits.len(),
            err = ?err,
            "failed to normalize rewritten workspace edit without file ops support"
        );
        return None;
    }

    match workspace_edit_to_lsp_with_uri_mapper(db, &canonical, |file| {
        Ok(join_uri(root_uri, Path::new(&file.0)))
    }) {
        Ok(edit) => Some(edit),
        Err(err) => {
            tracing::debug!(
                target = "nova.lsp",
                file_ops = canonical.file_ops.len(),
                text_edits = canonical.text_edits.len(),
                err = ?err,
                "failed to convert rewritten workspace edit without file ops support to LSP"
            );
            None
        }
    }
}

pub(crate) fn join_uri(root: &Uri, path: &Path) -> Uri {
    let mut uri = root.as_str().to_string();
    if !uri.ends_with('/') {
        uri.push('/');
    }

    for (idx, component) in path.components().enumerate() {
        if idx > 0 {
            uri.push('/');
        }
        let segment = component.as_os_str().to_string_lossy();
        uri.push_str(&encode_uri_segment(&segment));
    }

    match uri.parse() {
        Ok(uri) => uri,
        Err(err) => {
            tracing::error!(
                target = "nova.lsp",
                root = root.as_str(),
                path = %path.display(),
                uri,
                error = %err,
                "failed to join uri"
            );
            root.clone()
        }
    }
}

fn encode_uri_segment(segment: &str) -> String {
    // Encode using RFC 3986 unreserved set: ALPHA / DIGIT / "-" / "." / "_" / "~"
    let mut out = String::with_capacity(segment.len());
    for &b in segment.as_bytes() {
        if is_uri_unreserved(b) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(hex_digit(b >> 4));
            out.push(hex_digit(b & 0x0F));
        }
    }
    out
}

fn is_uri_unreserved(b: u8) -> bool {
    matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~')
}

fn hex_digit(n: u8) -> char {
    debug_assert!(n < 16, "nibble out of range: {n}");
    if n < 10 {
        (b'0' + n) as char
    } else {
        (b'A' + (n - 10)) as char
    }
}

pub(crate) fn full_document_range(contents: &str) -> lsp_types::Range {
    let end = end_position(contents);
    lsp_types::Range {
        start: lsp_types::Position {
            line: 0,
            character: 0,
        },
        end,
    }
}

fn end_position(contents: &str) -> lsp_types::Position {
    crate::offset_to_position(contents, contents.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    use lsp_types::{
        DocumentChangeOperation, DocumentChanges, ResourceOp, WorkspaceClientCapabilities,
        WorkspaceEditClientCapabilities,
    };
    use nova_refactor::MoveClassParams;
    use std::collections::BTreeMap;

    fn basic_move_edit(
        original_files: &HashMap<PathBuf, String>,
        from_path: &str,
        to_path: &str,
        new_contents: &str,
    ) -> RefactorWorkspaceEdit {
        let from = RefactorFileId::new(from_path);
        let to = RefactorFileId::new(to_path);
        let old_contents = match original_files.get(&PathBuf::from(from_path)) {
            Some(contents) => contents.as_str(),
            None => "",
        };

        RefactorWorkspaceEdit {
            file_ops: vec![RefactorFileOp::Rename {
                from: from.clone(),
                to: to.clone(),
            }],
            text_edits: vec![WorkspaceTextEdit::replace(
                to,
                nova_refactor::TextRange::new(0, old_contents.len()),
                new_contents,
            )],
        }
    }

    #[test]
    fn workspace_edit_includes_rename_operation_when_supported() {
        let mut original = HashMap::new();
        original.insert(
            PathBuf::from("src/main/java/com/foo/A.java"),
            "package com.foo;\n\npublic class A {}\n".to_string(),
        );

        let edit = basic_move_edit(
            &original,
            "src/main/java/com/foo/A.java",
            "src/main/java/com/bar/A.java",
            "package com.bar;\n\npublic class A {}\n",
        );

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

        let db = TextDatabase::new(original.iter().map(|(path, text)| {
            (
                RefactorFileId::new(path.to_string_lossy().into_owned()),
                text.clone(),
            )
        }));
        let root: Uri = "file:///workspace/".parse().unwrap();
        let ws_edit = workspace_edit_from_refactor_workspace_edit(&root, &db, &edit, &caps);

        let Some(DocumentChanges::Operations(ops)) = ws_edit.document_changes else {
            panic!("expected document change operations");
        };
        assert!(ops
            .iter()
            .any(|op| matches!(op, DocumentChangeOperation::Op(ResourceOp::Rename(_)))));
    }

    #[test]
    fn workspace_edit_falls_back_to_text_edits_without_file_ops_support() {
        let mut original = HashMap::new();
        original.insert(
            PathBuf::from("src/main/java/com/foo/A.java"),
            "package com.foo;\n\npublic class A {}\n".to_string(),
        );

        let edit = basic_move_edit(
            &original,
            "src/main/java/com/foo/A.java",
            "src/main/java/com/bar/A.java",
            "package com.bar;\n\npublic class A {}\n",
        );

        let caps = ClientCapabilities::default();

        let db = TextDatabase::new(original.iter().map(|(path, text)| {
            (
                RefactorFileId::new(path.to_string_lossy().into_owned()),
                text.clone(),
            )
        }));
        let root: Uri = "file:///workspace/".parse().unwrap();
        let ws_edit = workspace_edit_from_refactor_workspace_edit(&root, &db, &edit, &caps);

        assert!(ws_edit.document_changes.is_none());
        let changes = ws_edit.changes.expect("expected changes map");
        let uri = join_uri(&root, Path::new("src/main/java/com/foo/A.java"));
        assert!(changes.contains_key(&uri));
        assert!(changes[&uri][0].new_text.contains("package com.bar;"));
    }

    #[test]
    fn join_uri_percent_encodes_path_segments() {
        let root: Uri = "file:///workspace/".parse().unwrap();
        let uri = join_uri(&root, Path::new("src/main/java/com/foo/My File.java"));
        assert_eq!(
            uri.as_str(),
            "file:///workspace/src/main/java/com/foo/My%20File.java"
        );
    }

    #[test]
    fn workspace_edit_from_move_class_includes_rename_operation_when_supported() {
        let mut files = BTreeMap::new();
        files.insert(
            PathBuf::from("src/main/java/com/foo/A.java"),
            "package com.foo;\n\npublic class A {}\n".to_string(),
        );
        files.insert(
            PathBuf::from("src/main/java/com/other/C.java"),
            "package com.other;\n\nimport com.foo.A;\n\npublic class C { A a; }\n".to_string(),
        );

        let refactor = nova_refactor::move_class_workspace_edit(
            &files,
            MoveClassParams {
                source_path: PathBuf::from("src/main/java/com/foo/A.java"),
                class_name: "A".into(),
                target_package: "com.bar".into(),
            },
        )
        .unwrap();

        let db = TextDatabase::new(files.iter().map(|(path, text)| {
            (
                RefactorFileId::new(path.to_string_lossy().into_owned()),
                text.clone(),
            )
        }));

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
        let ws_edit = workspace_edit_from_refactor_workspace_edit(&root, &db, &refactor, &caps);

        let Some(DocumentChanges::Operations(ops)) = ws_edit.document_changes else {
            panic!("expected document change operations");
        };
        assert!(ops
            .iter()
            .any(|op| matches!(op, DocumentChangeOperation::Op(ResourceOp::Rename(_)))));
    }

    #[test]
    fn workspace_edit_uses_create_delete_when_rename_not_supported() {
        let mut original = HashMap::new();
        original.insert(
            PathBuf::from("src/main/java/com/foo/A.java"),
            "package com.foo;\n\npublic class A {}\n".to_string(),
        );

        let edit = basic_move_edit(
            &original,
            "src/main/java/com/foo/A.java",
            "src/main/java/com/bar/A.java",
            "package com.bar;\n\npublic class A {}\n",
        );

        let caps = ClientCapabilities {
            workspace: Some(WorkspaceClientCapabilities {
                workspace_edit: Some(WorkspaceEditClientCapabilities {
                    document_changes: Some(true),
                    resource_operations: Some(vec![
                        ResourceOperationKind::Create,
                        ResourceOperationKind::Delete,
                    ]),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let db = TextDatabase::new(original.iter().map(|(path, text)| {
            (
                RefactorFileId::new(path.to_string_lossy().into_owned()),
                text.clone(),
            )
        }));
        let root: Uri = "file:///workspace/".parse().unwrap();
        let ws_edit = workspace_edit_from_refactor_workspace_edit(&root, &db, &edit, &caps);

        let Some(DocumentChanges::Operations(ops)) = ws_edit.document_changes else {
            panic!("expected document change operations");
        };

        assert!(ops
            .iter()
            .any(|op| matches!(op, DocumentChangeOperation::Op(ResourceOp::Create(_)))));
        assert!(ops
            .iter()
            .any(|op| matches!(op, DocumentChangeOperation::Op(ResourceOp::Delete(_)))));
        assert!(!ops
            .iter()
            .any(|op| matches!(op, DocumentChangeOperation::Op(ResourceOp::Rename(_)))));
    }
}
