use std::collections::{BTreeMap, HashMap};

use lsp_types::{
    CodeAction, CodeActionKind, DocumentChangeOperation, DocumentChanges, OptionalVersionedTextDocumentIdentifier,
    Position, Range, ResourceOp, TextDocumentEdit, TextEdit as LspTextEdit, Uri, WorkspaceEdit as LspWorkspaceEdit,
};
use thiserror::Error;

use crate::edit::{apply_workspace_edit, FileId, FileOp, TextEdit, WorkspaceEdit};
use crate::java::SymbolId;
use crate::semantic::{Reference, RefactorDatabase, SymbolDefinition};

/// Minimal [`RefactorDatabase`] implementation backed by raw file text.
///
/// This is useful for converting canonical [`WorkspaceEdit`] values to LSP edits in layers that
/// don't yet have access to Nova's full semantic database.
#[derive(Clone, Debug, Default)]
pub struct TextDatabase {
    files: BTreeMap<FileId, String>,
}

impl TextDatabase {
    pub fn new(files: impl IntoIterator<Item = (FileId, String)>) -> Self {
        Self {
            files: files.into_iter().collect(),
        }
    }
}

impl RefactorDatabase for TextDatabase {
    fn file_text(&self, file: &FileId) -> Option<&str> {
        self.files.get(file).map(String::as_str)
    }

    fn symbol_definition(&self, _symbol: SymbolId) -> Option<SymbolDefinition> {
        None
    }

    fn symbol_scope(&self, _symbol: SymbolId) -> Option<u32> {
        None
    }

    fn resolve_name_in_scope(&self, _scope: u32, _name: &str) -> Option<SymbolId> {
        None
    }

    fn would_shadow(&self, _scope: u32, _name: &str) -> Option<SymbolId> {
        None
    }

    fn find_references(&self, _symbol: SymbolId) -> Vec<Reference> {
        Vec::new()
    }
}

#[derive(Debug, Error)]
pub enum LspConversionError {
    #[error("unknown file {0:?}")]
    UnknownFile(FileId),
    #[error("invalid uri for {0:?}")]
    InvalidUri(FileId),
    #[error("workspace edit contains file operations; use documentChanges conversion")]
    FileOpsRequireDocumentChanges,
    #[error(transparent)]
    Edit(#[from] crate::edit::EditError),
}

/// Convert an internal [`WorkspaceEdit`] into an LSP [`WorkspaceEdit`].
pub fn workspace_edit_to_lsp(
    db: &dyn RefactorDatabase,
    edit: &WorkspaceEdit,
) -> Result<LspWorkspaceEdit, LspConversionError> {
    if !edit.file_ops.is_empty() {
        return Err(LspConversionError::FileOpsRequireDocumentChanges);
    }

    let mut normalized = edit.clone();
    normalized.normalize()?;

    let mut changes: HashMap<Uri, Vec<LspTextEdit>> = HashMap::new();

    for e in &normalized.text_edits {
        let text = db
            .file_text(&e.file)
            .ok_or_else(|| LspConversionError::UnknownFile(e.file.clone()))?;
        let uri = file_id_to_uri(&e.file)?;

        let range = Range {
            start: offset_to_position(text, e.range.start),
            end: offset_to_position(text, e.range.end),
        };

        changes.entry(uri).or_default().push(LspTextEdit {
            range,
            new_text: e.replacement.clone(),
        });
    }

    // LSP clients tend to apply edits sequentially. Provide them in reverse
    // order to avoid offset shifting even if a client ignores the spec.
    for edits in changes.values_mut() {
        edits.sort_by(|a, b| {
            b.range
                .start
                .line
                .cmp(&a.range.start.line)
                .then_with(|| b.range.start.character.cmp(&a.range.start.character))
                .then_with(|| b.range.end.line.cmp(&a.range.end.line))
                .then_with(|| b.range.end.character.cmp(&a.range.end.character))
        });
    }

    Ok(LspWorkspaceEdit {
        changes: Some(changes),
        document_changes: None,
        change_annotations: None,
    })
}

/// Convert a [`WorkspaceEdit`] into an LSP [`WorkspaceEdit`] using `documentChanges`.
///
/// This is required to represent file operations (rename/create/delete).
pub fn workspace_edit_to_lsp_document_changes(
    db: &dyn RefactorDatabase,
    edit: &WorkspaceEdit,
) -> Result<LspWorkspaceEdit, LspConversionError> {
    let mut normalized = edit.clone();
    normalized.normalize()?;

    // Build the pre-text-edit file map (after file ops) so we can compute UTF-16 ranges for edits
    // that target renamed / created files.
    let files_after_ops = file_texts_after_file_ops(db, &normalized)?;

    let mut edits_by_file: BTreeMap<FileId, Vec<TextEdit>> = BTreeMap::new();
    for e in &normalized.text_edits {
        edits_by_file
            .entry(e.file.clone())
            .or_default()
            .push(e.clone());
    }

    let mut ops: Vec<DocumentChangeOperation> = Vec::new();

    // 1) File operations, in order.
    for op in &normalized.file_ops {
        match op {
            FileOp::Rename { from, to } => {
                ops.push(DocumentChangeOperation::Op(ResourceOp::Rename(
                    lsp_types::RenameFile {
                        old_uri: file_id_to_uri(from)?,
                        new_uri: file_id_to_uri(to)?,
                        options: None,
                        annotation_id: None,
                    },
                )));
            }
            FileOp::Delete { file } => {
                ops.push(DocumentChangeOperation::Op(ResourceOp::Delete(
                    lsp_types::DeleteFile {
                        uri: file_id_to_uri(file)?,
                        options: None,
                    },
                )));
            }
            FileOp::Create { file, contents } => {
                let uri = file_id_to_uri(file)?;
                ops.push(DocumentChangeOperation::Op(ResourceOp::Create(
                    lsp_types::CreateFile {
                        uri: uri.clone(),
                        options: None,
                        annotation_id: None,
                    },
                )));

                // LSP CreateFile doesn't include contents; represent "create with contents" as a
                // full rewrite of an empty document. If there are additional text edits on this
                // file, apply them in-memory and send a single rewrite to avoid offset shifting.
                let file_edits = edits_by_file.remove(file).unwrap_or_default();
                let final_contents =
                    crate::edit::apply_text_edits(contents, &file_edits).map_err(LspConversionError::Edit)?;

                ops.push(DocumentChangeOperation::Edit(TextDocumentEdit {
                    text_document: OptionalVersionedTextDocumentIdentifier {
                        uri,
                        version: None,
                    },
                    edits: vec![lsp_types::OneOf::Left(LspTextEdit {
                        range: full_document_range(""),
                        new_text: final_contents,
                    })],
                }));
            }
        }
    }

    // 2) Text edits for existing (non-created) files, grouped by file in deterministic order.
    for (file, mut edits) in edits_by_file {
        let text = files_after_ops
            .get(&file)
            .ok_or_else(|| LspConversionError::UnknownFile(file.clone()))?;
        let uri = file_id_to_uri(&file)?;

        // Convert byte offsets -> UTF-16 positions.
        let mut lsp_edits: Vec<lsp_types::OneOf<LspTextEdit, lsp_types::AnnotatedTextEdit>> =
            edits
                .drain(..)
                .map(|e| {
                    let range = Range {
                        start: offset_to_position(text, e.range.start),
                        end: offset_to_position(text, e.range.end),
                    };
                    lsp_types::OneOf::Left(LspTextEdit {
                        range,
                        new_text: e.replacement,
                    })
                })
                .collect();

        // LSP clients tend to apply edits sequentially. Provide them in reverse order to avoid
        // offset shifting even if a client ignores the spec.
        lsp_edits.sort_by(|a, b| {
            let (a, b) = match (a, b) {
                (lsp_types::OneOf::Left(a), lsp_types::OneOf::Left(b)) => (a, b),
                _ => return std::cmp::Ordering::Equal,
            };
            b.range
                .start
                .line
                .cmp(&a.range.start.line)
                .then_with(|| b.range.start.character.cmp(&a.range.start.character))
                .then_with(|| b.range.end.line.cmp(&a.range.end.line))
                .then_with(|| b.range.end.character.cmp(&a.range.end.character))
        });

        ops.push(DocumentChangeOperation::Edit(TextDocumentEdit {
            text_document: OptionalVersionedTextDocumentIdentifier { uri, version: None },
            edits: lsp_edits,
        }));
    }

    Ok(LspWorkspaceEdit {
        changes: None,
        document_changes: Some(DocumentChanges::Operations(ops)),
        change_annotations: None,
    })
}

pub fn code_action_for_edit(
    title: impl Into<String>,
    kind: CodeActionKind,
    edit: LspWorkspaceEdit,
) -> CodeAction {
    CodeAction {
        title: title.into(),
        kind: Some(kind),
        edit: Some(edit),
        command: None,
        diagnostics: None,
        is_preferred: Some(true),
        disabled: None,
        data: None,
    }
}

fn file_id_to_uri(file: &FileId) -> Result<Uri, LspConversionError> {
    file.0
        .parse::<Uri>()
        .map_err(|_| LspConversionError::InvalidUri(file.clone()))
}

fn offset_to_position(text: &str, offset: usize) -> Position {
    let mut line: u32 = 0;
    let mut col_utf16: u32 = 0;

    let mut i = 0;
    for ch in text.chars() {
        if i >= offset {
            break;
        }

        if ch == '\n' {
            line += 1;
            col_utf16 = 0;
        } else {
            col_utf16 += ch.len_utf16() as u32;
        }

        i += ch.len_utf8();
    }

    Position {
        line,
        character: col_utf16,
    }
}

fn full_document_range(contents: &str) -> Range {
    let end = offset_to_position(contents, contents.len());
    Range {
        start: Position { line: 0, character: 0 },
        end,
    }
}

fn file_texts_after_file_ops(
    db: &dyn RefactorDatabase,
    edit: &WorkspaceEdit,
) -> Result<BTreeMap<FileId, String>, LspConversionError> {
    let mut files: BTreeMap<FileId, String> = BTreeMap::new();

    // Seed with known file contents.
    for op in &edit.file_ops {
        match op {
            FileOp::Rename { from, to } => {
                let text = db
                    .file_text(from)
                    .ok_or_else(|| LspConversionError::UnknownFile(from.clone()))?;
                files.insert(from.clone(), text.to_string());

                if let Some(text) = db.file_text(to) {
                    files.insert(to.clone(), text.to_string());
                }
            }
            FileOp::Delete { file } => {
                let text = db
                    .file_text(file)
                    .ok_or_else(|| LspConversionError::UnknownFile(file.clone()))?;
                files.insert(file.clone(), text.to_string());
            }
            FileOp::Create { file, .. } => {
                // Include existing content to surface create conflicts.
                if let Some(text) = db.file_text(file) {
                    files.insert(file.clone(), text.to_string());
                }
            }
        }
    }

    for e in &edit.text_edits {
        if files.contains_key(&e.file) {
            continue;
        }
        if let Some(text) = db.file_text(&e.file) {
            files.insert(e.file.clone(), text.to_string());
        }
    }

    // Apply only file ops (no text edits) to get the pre-text-edit workspace.
    let ops_only = WorkspaceEdit {
        file_ops: edit.file_ops.clone(),
        text_edits: Vec::new(),
    };
    let out = apply_workspace_edit(&files, &ops_only)?;
    Ok(out)
}

// Keep this here so callers don't need to import our internal types when
// building code actions around plain text edits.
#[allow(dead_code)]
fn _lsp_text_edit(
    db: &dyn RefactorDatabase,
    edit: &TextEdit,
) -> Result<LspTextEdit, LspConversionError> {
    let text = db
        .file_text(&edit.file)
        .ok_or_else(|| LspConversionError::UnknownFile(edit.file.clone()))?;
    Ok(LspTextEdit {
        range: Range {
            start: offset_to_position(text, edit.range.start),
            end: offset_to_position(text, edit.range.end),
        },
        new_text: edit.replacement.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semantic::{Reference, RefactorDatabase, SymbolDefinition};
    use crate::java::SymbolId;
    use pretty_assertions::assert_eq;

    #[derive(Default)]
    struct TestDb {
        files: BTreeMap<FileId, String>,
    }

    impl RefactorDatabase for TestDb {
        fn file_text(&self, file: &FileId) -> Option<&str> {
            self.files.get(file).map(String::as_str)
        }

        fn symbol_definition(&self, _symbol: SymbolId) -> Option<SymbolDefinition> {
            None
        }

        fn symbol_scope(&self, _symbol: SymbolId) -> Option<u32> {
            None
        }

        fn resolve_name_in_scope(&self, _scope: u32, _name: &str) -> Option<SymbolId> {
            None
        }

        fn would_shadow(&self, _scope: u32, _name: &str) -> Option<SymbolId> {
            None
        }

        fn find_references(&self, _symbol: SymbolId) -> Vec<Reference> {
            Vec::new()
        }
    }

    #[test]
    fn document_changes_includes_rename_and_utf16_correct_ranges() {
        let old_file = FileId::new("file:///old.txt");
        let new_file = FileId::new("file:///new.txt");

        let mut db = TestDb::default();
        db.files.insert(old_file.clone(), "a😀b\n".to_string());

        let edit = WorkspaceEdit {
            file_ops: vec![FileOp::Rename {
                from: old_file.clone(),
                to: new_file.clone(),
            }],
            // Replace the 😀 (byte offsets 1..5) with X.
            text_edits: vec![TextEdit::replace(
                new_file.clone(),
                crate::edit::TextRange::new(1, 5),
                "X",
            )],
        };

        let lsp = workspace_edit_to_lsp_document_changes(&db, &edit).unwrap();
        let Some(DocumentChanges::Operations(ops)) = lsp.document_changes else {
            panic!("expected DocumentChanges::Operations");
        };

        assert!(
            ops.iter().any(|op| matches!(
                op,
                DocumentChangeOperation::Op(ResourceOp::Rename(_))
            )),
            "expected a rename op, got: {ops:?}"
        );

        let text_doc_edit = ops
            .iter()
            .find_map(|op| match op {
                DocumentChangeOperation::Edit(e) => Some(e),
                _ => None,
            })
            .expect("expected a TextDocumentEdit");

        assert_eq!(text_doc_edit.text_document.uri.as_str(), new_file.0.as_str());

        let edit = match &text_doc_edit.edits[0] {
            lsp_types::OneOf::Left(e) => e,
            lsp_types::OneOf::Right(e) => &e.text_edit,
        };

        assert_eq!(edit.range.start, Position { line: 0, character: 1 });
        assert_eq!(edit.range.end, Position { line: 0, character: 3 });
        assert_eq!(edit.new_text, "X");
    }
}
