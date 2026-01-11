use std::collections::HashMap;

use lsp_types::{
    CodeAction, CodeActionKind, Position, Range, TextEdit as LspTextEdit, Uri,
    WorkspaceEdit as LspWorkspaceEdit,
};
use thiserror::Error;

use crate::edit::{FileId, TextEdit, WorkspaceEdit};
use crate::semantic::RefactorDatabase;

#[derive(Debug, Error)]
pub enum LspConversionError {
    #[error("unknown file {0:?}")]
    UnknownFile(FileId),
    #[error("invalid uri for {0:?}")]
    InvalidUri(FileId),
}

/// Convert an internal [`WorkspaceEdit`] into an LSP [`WorkspaceEdit`].
pub fn workspace_edit_to_lsp(
    db: &dyn RefactorDatabase,
    edit: &WorkspaceEdit,
) -> Result<LspWorkspaceEdit, LspConversionError> {
    let mut changes: HashMap<Uri, Vec<LspTextEdit>> = HashMap::new();

    for e in &edit.edits {
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
