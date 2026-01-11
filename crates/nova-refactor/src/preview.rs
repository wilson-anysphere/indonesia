use std::collections::BTreeMap;

use similar::TextDiff;

use crate::edit::{apply_text_edits, FileId, TextEdit, WorkspaceEdit};
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
    let mut by_file: BTreeMap<&FileId, Vec<TextEdit>> = BTreeMap::new();
    for e in &edit.edits {
        by_file.entry(&e.file).or_default().push(e.clone());
    }

    let mut files = Vec::new();
    for (file, edits) in by_file {
        let original = db.file_text(file).unwrap_or_default().to_string();
        let modified = apply_text_edits(&original, &edits)?;

        let diff = TextDiff::from_lines(&original, &modified);
        let unified_diff = diff
            .unified_diff()
            .context_radius(3)
            .header(&format!("a/{}", file.0), &format!("b/{}", file.0))
            .to_string();

        files.push(FilePreview {
            file: file.clone(),
            original,
            modified,
            unified_diff,
            edit_count: edits.len(),
        });
    }

    Ok(RefactoringPreview {
        total_files: files.len(),
        total_edits: edit.edits.len(),
        files,
    })
}
