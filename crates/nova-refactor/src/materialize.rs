use thiserror::Error;

use crate::edit::{EditError, TextEdit, TextRange, WorkspaceEdit};
use crate::semantic::{RefactorDatabase, SemanticChange};

#[derive(Debug, Error, PartialEq, Eq)]
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

fn file_text<'a>(
    db: &'a dyn RefactorDatabase,
    file: &crate::edit::FileId,
) -> Result<&'a str, MaterializeError> {
    db.file_text(file)
        .ok_or_else(|| MaterializeError::UnknownFile(file.clone()))
}

fn validate_offset(
    db: &dyn RefactorDatabase,
    file: &crate::edit::FileId,
    offset: usize,
) -> Result<(), MaterializeError> {
    let text = file_text(db, file)?;
    if offset > text.len() {
        return Err(MaterializeError::InvalidRange {
            file: file.clone(),
            range: TextRange::new(offset, offset),
            len: text.len(),
        });
    }
    if !text.is_char_boundary(offset) {
        return Err(EditError::InvalidUtf8Boundary {
            file: file.clone(),
            offset,
        }
        .into());
    }
    Ok(())
}

fn validate_range(
    db: &dyn RefactorDatabase,
    file: &crate::edit::FileId,
    range: TextRange,
) -> Result<(), MaterializeError> {
    let text = file_text(db, file)?;
    if range.start > range.end || range.end > text.len() {
        return Err(MaterializeError::InvalidRange {
            file: file.clone(),
            range,
            len: text.len(),
        });
    }
    if !text.is_char_boundary(range.start) {
        return Err(EditError::InvalidUtf8Boundary {
            file: file.clone(),
            offset: range.start,
        }
        .into());
    }
    if !text.is_char_boundary(range.end) {
        return Err(EditError::InvalidUtf8Boundary {
            file: file.clone(),
            offset: range.end,
        }
        .into());
    }
    Ok(())
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

                validate_range(db, &def.file, def.name_range)?;
                edits.push(TextEdit::replace(
                    def.file.clone(),
                    def.name_range,
                    new_name.clone(),
                ));

                for reference in db.find_references(symbol) {
                    validate_range(db, &reference.file, reference.range)?;
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
                validate_range(db, &file, range)?;
                validate_offset(db, &target_file, target_offset)?;

                let text = file_text(db, &file)?;
                let moved = text[range.start..range.end].to_string();
                edits.push(TextEdit::delete(file.clone(), range));
                edits.push(TextEdit::insert(target_file, target_offset, moved));
            }
            SemanticChange::Add { file, offset, text } => {
                validate_offset(db, &file, offset)?;
                edits.push(TextEdit::insert(file, offset, text));
            }
            SemanticChange::Remove { file, range } => {
                validate_range(db, &file, range)?;
                edits.push(TextEdit::delete(file, range));
            }
            SemanticChange::Replace { file, range, text } => {
                validate_range(db, &file, range)?;
                edits.push(TextEdit::replace(file, range, text));
            }
            SemanticChange::ChangeType {
                file,
                range,
                new_type,
            } => {
                validate_range(db, &file, range)?;
                edits.push(TextEdit::replace(file, range, new_type));
            }
            SemanticChange::UpdateReferences {
                file,
                range,
                new_text,
            } => {
                validate_range(db, &file, range)?;
                edits.push(TextEdit::replace(file, range, new_text));
            }
        }
    }

    let mut edit = WorkspaceEdit::new(edits);
    edit.normalize()?;
    Ok(edit)
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};

    use pretty_assertions::assert_eq;

    use super::*;
    use crate::edit::FileId;
    use crate::java::SymbolId;
    use crate::semantic::{Reference, ReferenceKind, SymbolDefinition};

    #[derive(Default)]
    struct TestDb {
        files: BTreeMap<FileId, String>,
        defs: HashMap<SymbolId, SymbolDefinition>,
        refs: HashMap<SymbolId, Vec<Reference>>,
    }

    impl RefactorDatabase for TestDb {
        fn file_text(&self, file: &FileId) -> Option<&str> {
            self.files.get(file).map(|t| t.as_str())
        }

        fn symbol_definition(&self, symbol: SymbolId) -> Option<SymbolDefinition> {
            self.defs.get(&symbol).cloned()
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

        fn find_references(&self, symbol: SymbolId) -> Vec<Reference> {
            self.refs.get(&symbol).cloned().unwrap_or_default()
        }
    }

    #[test]
    fn replace_out_of_bounds_returns_invalid_range() {
        let file = FileId::new("file:///test");
        let mut db = TestDb::default();
        db.files.insert(file.clone(), "abc".to_string());

        let err = materialize(
            &db,
            [SemanticChange::Replace {
                file: file.clone(),
                range: TextRange::new(0, 10),
                text: "x".to_string(),
            }],
        )
        .unwrap_err();

        assert_eq!(
            err,
            MaterializeError::InvalidRange {
                file,
                range: TextRange::new(0, 10),
                len: 3
            }
        );
    }

    #[test]
    fn add_out_of_bounds_returns_invalid_range() {
        let file = FileId::new("file:///test");
        let mut db = TestDb::default();
        db.files.insert(file.clone(), "abc".to_string());

        let err = materialize(
            &db,
            [SemanticChange::Add {
                file: file.clone(),
                offset: 4,
                text: "x".to_string(),
            }],
        )
        .unwrap_err();

        assert_eq!(
            err,
            MaterializeError::InvalidRange {
                file,
                range: TextRange::new(4, 4),
                len: 3
            }
        );
    }

    #[test]
    fn remove_out_of_bounds_returns_invalid_range() {
        let file = FileId::new("file:///test");
        let mut db = TestDb::default();
        db.files.insert(file.clone(), "abc".to_string());

        let err = materialize(
            &db,
            [SemanticChange::Remove {
                file: file.clone(),
                range: TextRange::new(0, 10),
            }],
        )
        .unwrap_err();

        assert_eq!(
            err,
            MaterializeError::InvalidRange {
                file,
                range: TextRange::new(0, 10),
                len: 3
            }
        );
    }

    #[test]
    fn change_type_out_of_bounds_returns_invalid_range() {
        let file = FileId::new("file:///test");
        let mut db = TestDb::default();
        db.files.insert(file.clone(), "abc".to_string());

        let err = materialize(
            &db,
            [SemanticChange::ChangeType {
                file: file.clone(),
                range: TextRange::new(0, 10),
                new_type: "int".to_string(),
            }],
        )
        .unwrap_err();

        assert_eq!(
            err,
            MaterializeError::InvalidRange {
                file,
                range: TextRange::new(0, 10),
                len: 3
            }
        );
    }

    #[test]
    fn update_references_out_of_bounds_returns_invalid_range() {
        let file = FileId::new("file:///test");
        let mut db = TestDb::default();
        db.files.insert(file.clone(), "abc".to_string());

        let err = materialize(
            &db,
            [SemanticChange::UpdateReferences {
                file: file.clone(),
                range: TextRange::new(0, 10),
                new_text: "x".to_string(),
            }],
        )
        .unwrap_err();

        assert_eq!(
            err,
            MaterializeError::InvalidRange {
                file,
                range: TextRange::new(0, 10),
                len: 3
            }
        );
    }

    #[test]
    fn rename_out_of_bounds_reference_returns_invalid_range() {
        let file = FileId::new("file:///test");
        let mut db = TestDb::default();
        db.files.insert(file.clone(), "abc".to_string());

        let symbol = SymbolId::new(0);
        db.defs.insert(
            symbol,
            SymbolDefinition {
                file: file.clone(),
                name: "a".to_string(),
                name_range: TextRange::new(0, 1),
                scope: 0,
            },
        );
        db.refs.insert(
            symbol,
            vec![Reference {
                file: file.clone(),
                range: TextRange::new(10, 11),
                scope: None,
                kind: ReferenceKind::Name,
            }],
        );

        let err = materialize(
            &db,
            [SemanticChange::Rename {
                symbol,
                new_name: "b".to_string(),
            }],
        )
        .unwrap_err();

        assert_eq!(
            err,
            MaterializeError::InvalidRange {
                file,
                range: TextRange::new(10, 11),
                len: 3
            }
        );
    }

    #[test]
    fn move_invalid_utf8_boundary_returns_error_instead_of_panicking() {
        let src = FileId::new("file:///src");
        let dst = FileId::new("file:///dst");
        let mut db = TestDb::default();
        db.files.insert(src.clone(), "aé".to_string());
        db.files.insert(dst.clone(), "x".to_string());

        // `é` is 2 bytes in UTF-8; `2` is not a character boundary in "aé" (len=3).
        let err = materialize(
            &db,
            [SemanticChange::Move {
                file: src.clone(),
                range: TextRange::new(2, 3),
                target_file: dst,
                target_offset: 0,
            }],
        )
        .unwrap_err();

        assert_eq!(
            err,
            MaterializeError::Edit(EditError::InvalidUtf8Boundary {
                file: src,
                offset: 2,
            })
        );
    }

    #[test]
    fn rename_out_of_bounds_definition_returns_invalid_range() {
        let file = FileId::new("file:///test");
        let mut db = TestDb::default();
        db.files.insert(file.clone(), "abc".to_string());

        let symbol = SymbolId::new(0);
        db.defs.insert(
            symbol,
            SymbolDefinition {
                file: file.clone(),
                name: "a".to_string(),
                name_range: TextRange::new(10, 11),
                scope: 0,
            },
        );

        let err = materialize(
            &db,
            [SemanticChange::Rename {
                symbol,
                new_name: "b".to_string(),
            }],
        )
        .unwrap_err();

        assert_eq!(
            err,
            MaterializeError::InvalidRange {
                file,
                range: TextRange::new(10, 11),
                len: 3
            }
        );
    }
}
