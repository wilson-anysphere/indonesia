use std::collections::{BTreeMap, BTreeSet};

use similar::TextDiff;

use crate::edit::{apply_workspace_edit, FileId, FileOp, WorkspaceEdit};
use crate::semantic::RefactorDatabase;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileChangeKind {
    Created,
    Deleted,
    Modified,
    Renamed { from: FileId, to: FileId },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilePreview {
    pub file: FileId,
    pub change: FileChangeKind,
    pub original: String,
    pub modified: String,
    pub unified_diff: String,
    pub edit_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefactoringPreview {
    pub total_files: usize,
    pub total_edits: usize,
    pub file_ops: Vec<FileOp>,
    pub files: Vec<FilePreview>,
}

fn file_text_or_empty<'a>(files: &'a BTreeMap<FileId, String>, file: &FileId) -> &'a str {
    files.get(file).map(String::as_str).unwrap_or("")
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

    // Rename mapping (destination -> source) so we can present renames as a single file diff
    // instead of "delete old + create new".
    let mut rename_dests: BTreeMap<FileId, FileId> = BTreeMap::new();
    let mut rename_sources: BTreeSet<FileId> = BTreeSet::new();
    let mut rename_targets: BTreeSet<FileId> = BTreeSet::new();
    for op in &normalized.file_ops {
        let FileOp::Rename { from, to } = op else {
            continue;
        };
        rename_dests.insert(to.clone(), from.clone());
        rename_sources.insert(from.clone());
        rename_targets.insert(to.clone());
    }

    let mut all_files: BTreeSet<FileId> = BTreeSet::new();
    all_files.extend(original_files.keys().cloned());
    all_files.extend(modified_files.keys().cloned());

    // A pure rename moves contents from `from` to `to`. Showing the `from` path as "deleted" is
    // confusing; treat the rename as a diff shown at the destination path instead.
    for from in rename_sources.difference(&rename_targets) {
        all_files.remove(from);
    }

    let mut files = Vec::new();
    for file in all_files {
        let (change, original, modified, header_from, header_to) =
            if let Some(from) = rename_dests.get(&file) {
                (
                    FileChangeKind::Renamed {
                        from: from.clone(),
                        to: file.clone(),
                    },
                    file_text_or_empty(&original_files, from),
                    file_text_or_empty(&modified_files, &file),
                    from.0.clone(),
                    file.0.clone(),
                )
            } else {
                let original = file_text_or_empty(&original_files, &file);
                let modified = file_text_or_empty(&modified_files, &file);

                let change = match (
                    original_files.contains_key(&file),
                    modified_files.contains_key(&file),
                ) {
                    (false, true) => FileChangeKind::Created,
                    (true, false) => FileChangeKind::Deleted,
                    _ => FileChangeKind::Modified,
                };

                (change, original, modified, file.0.clone(), file.0.clone())
            };
        if original == modified {
            continue;
        }

        let diff = TextDiff::from_lines(original, modified);
        let unified_diff = diff
            .unified_diff()
            .context_radius(3)
            .header(&format!("a/{}", header_from), &format!("b/{}", header_to))
            .to_string();

        let edit_count = normalized
            .text_edits
            .iter()
            .filter(|e| e.file == file)
            .count();

        files.push(FilePreview {
            file: file.clone(),
            change,
            original: original.to_string(),
            modified: modified.to_string(),
            unified_diff,
            edit_count,
        });
    }

    Ok(RefactoringPreview {
        total_files: files.len(),
        total_edits: normalized.text_edits.len(),
        file_ops: normalized.file_ops.clone(),
        files,
    })
}
