use thiserror::Error;

use crate::edit::{TextEdit, TextRange, WorkspaceEdit};
use crate::semantic::{RefactorDatabase, SemanticChange};

#[derive(Debug, Error)]
pub enum MaterializeError {
    #[error("unknown symbol {0:?}")]
    UnknownSymbol(crate::java::SymbolId),
    #[error("unknown file {0:?}")]
    UnknownFile(crate::edit::FileId),
    #[error("invalid range {range:?} for file {file:?} (len={len})")]
    InvalidRange {
        file: crate::edit::FileId,
        range: TextRange,
        len: usize,
    },
    #[error(transparent)]
    Edit(#[from] crate::edit::EditError),
}

/// Convert semantic changes into a deterministic, non-overlapping [`WorkspaceEdit`].
pub fn materialize(
    db: &dyn RefactorDatabase,
    changes: impl IntoIterator<Item = SemanticChange>,
) -> Result<WorkspaceEdit, MaterializeError> {
    let mut edits: Vec<TextEdit> = Vec::new();

    for change in changes {
        match change {
            SemanticChange::Rename { symbol, new_name } => {
                let def = db
                    .symbol_definition(symbol)
                    .ok_or(MaterializeError::UnknownSymbol(symbol))?;

                edits.push(TextEdit::replace(
                    def.file.clone(),
                    def.name_range,
                    new_name.clone(),
                ));

                for reference in db.find_references(symbol) {
                    edits.push(TextEdit::replace(
                        reference.file.clone(),
                        reference.range,
                        new_name.clone(),
                    ));
                }
            }
            SemanticChange::Move {
                file,
                range,
                target_file,
                target_offset,
            } => {
                let text = db
                    .file_text(&file)
                    .ok_or_else(|| MaterializeError::UnknownFile(file.clone()))?;

                if range.end > text.len() {
                    return Err(MaterializeError::InvalidRange {
                        file,
                        range,
                        len: text.len(),
                    });
                }

                let moved = text[range.start..range.end].to_string();
                edits.push(TextEdit::delete(file.clone(), range));
                edits.push(TextEdit::insert(target_file, target_offset, moved));
            }
            SemanticChange::Add { file, offset, text } => {
                edits.push(TextEdit::insert(file, offset, text));
            }
            SemanticChange::Remove { file, range } => {
                edits.push(TextEdit::delete(file, range));
            }
            SemanticChange::Replace { file, range, text } => {
                edits.push(TextEdit::replace(file, range, text));
            }
            SemanticChange::ChangeType {
                file,
                range,
                new_type,
            } => {
                edits.push(TextEdit::replace(file, range, new_type));
            }
            SemanticChange::UpdateReferences {
                file,
                range,
                new_text,
            } => {
                edits.push(TextEdit::replace(file, range, new_text));
            }
        }
    }

    let mut edit = WorkspaceEdit::new(edits);
    edit.normalize()?;
    Ok(edit)
}

