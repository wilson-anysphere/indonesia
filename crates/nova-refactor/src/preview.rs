use std::collections::{BTreeMap, BTreeSet};

use similar::TextDiff;

use crate::edit::{apply_workspace_edit, FileId, WorkspaceEdit};
use crate::semantic::RefactorDatabase;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilePreview {
    pub file: FileId,
    pub original: String,
    pub modified: String,
    pub unified_diff: String,
    pub edit_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefactoringPreview {
    pub total_files: usize,
    pub total_edits: usize,
    pub files: Vec<FilePreview>,
}

pub fn generate_preview(
    db: &dyn RefactorDatabase,
    edit: &WorkspaceEdit,
) -> Result<RefactoringPreview, crate::edit::EditError> {
    let mut normalized = edit.clone();
    normalized.normalize()?;

    // Build a minimal in-memory snapshot containing just the files we can resolve from the
    // database. Missing entries are treated as non-existent and will be created via file ops.
    let mut original_files: BTreeMap<FileId, String> = BTreeMap::new();

    for op in &normalized.file_ops {
        match op {
            crate::edit::FileOp::Rename { from, to } => {
                if let Some(text) = db.file_text(from) {
                    original_files.insert(from.clone(), text.to_string());
                }
                // If the destination exists already, include it so `apply_workspace_edit` can
                // surface the collision.
                if let Some(text) = db.file_text(to) {
                    original_files.insert(to.clone(), text.to_string());
                }
            }
            crate::edit::FileOp::Create { file, .. } => {
                // Include existing content to surface create conflicts.
                if let Some(text) = db.file_text(file) {
                    original_files.insert(file.clone(), text.to_string());
                }
            }
            crate::edit::FileOp::Delete { file } => {
                if let Some(text) = db.file_text(file) {
                    original_files.insert(file.clone(), text.to_string());
                }
            }
        }
    }

    for e in &normalized.text_edits {
        if original_files.contains_key(&e.file) {
            continue;
        }
        if let Some(text) = db.file_text(&e.file) {
            original_files.insert(e.file.clone(), text.to_string());
        }
    }

    let modified_files = apply_workspace_edit(&original_files, &normalized)?;

    let mut all_files: BTreeSet<FileId> = BTreeSet::new();
    all_files.extend(original_files.keys().cloned());
    all_files.extend(modified_files.keys().cloned());

    let mut files = Vec::new();
    for file in all_files {
        let original = original_files.get(&file).cloned().unwrap_or_default();
        let modified = modified_files.get(&file).cloned().unwrap_or_default();
        if original == modified {
            continue;
        }

        let diff = TextDiff::from_lines(&original, &modified);
        let unified_diff = diff
            .unified_diff()
            .context_radius(3)
            .header(&format!("a/{}", file.0), &format!("b/{}", file.0))
            .to_string();

        let edit_count = normalized
            .text_edits
            .iter()
            .filter(|e| e.file == file)
            .count();

        files.push(FilePreview {
            file: file.clone(),
            original,
            modified,
            unified_diff,
            edit_count,
        });
    }

    Ok(RefactoringPreview {
        total_files: files.len(),
        total_edits: normalized.text_edits.len(),
        files,
    })
}
