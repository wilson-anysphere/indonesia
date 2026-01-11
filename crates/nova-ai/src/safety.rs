use crate::patch::{Patch, UnifiedDiffLine};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct PatchSafetyConfig {
    pub max_files: usize,
    pub max_total_inserted_chars: usize,
    pub excluded_path_prefixes: Vec<String>,
    pub no_new_imports: bool,
}

impl Default for PatchSafetyConfig {
    fn default() -> Self {
        Self {
            max_files: 10,
            max_total_inserted_chars: 20_000,
            excluded_path_prefixes: Vec::new(),
            no_new_imports: false,
        }
    }
}

#[derive(Debug, Error)]
pub enum SafetyError {
    #[error("patch touches too many files ({files} > {max})")]
    TooManyFiles { files: usize, max: usize },
    #[error("patch inserts too many characters ({chars} > {max})")]
    TooManyInsertedChars { chars: usize, max: usize },
    #[error("patch attempted to edit excluded path '{path}'")]
    ExcludedPath { path: String },
    #[error("patch attempted to use non-relative path '{path}'")]
    NonRelativePath { path: String },
    #[error("patch introduces new imports in '{file}': {imports:?}")]
    NewImports { file: String, imports: Vec<String> },
    #[error("unsupported unified diff patch: {0}")]
    UnsupportedUnifiedDiff(String),
}

pub fn enforce_patch_safety(patch: &Patch, config: &PatchSafetyConfig) -> Result<(), SafetyError> {
    let mut files = BTreeSet::new();
    let mut inserted_chars = 0usize;
    let mut new_imports: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    match patch {
        Patch::Edits(edits) => {
            for edit in edits {
                validate_path(&edit.file, config)?;
                files.insert(edit.file.clone());
                inserted_chars = inserted_chars.saturating_add(edit.text.len());

                if config.no_new_imports {
                    for line in edit.text.lines() {
                        if is_import_line(line) {
                            new_imports
                                .entry(edit.file.clone())
                                .or_default()
                                .insert(line.trim().to_string());
                        }
                    }
                }
            }
        }
        Patch::UnifiedDiff(diff) => {
            for file in &diff.files {
                if file.new_path == "/dev/null" {
                    return Err(SafetyError::UnsupportedUnifiedDiff(
                        "file deletions are not supported".into(),
                    ));
                }
                if file.old_path != "/dev/null" && file.old_path != file.new_path {
                    return Err(SafetyError::UnsupportedUnifiedDiff(
                        "file renames are not supported".into(),
                    ));
                }

                validate_path(&file.new_path, config)?;
                files.insert(file.new_path.clone());

                for hunk in &file.hunks {
                    for line in &hunk.lines {
                        if let UnifiedDiffLine::Add(text) = line {
                            inserted_chars = inserted_chars.saturating_add(text.len());
                            if config.no_new_imports && is_import_line(text) {
                                new_imports
                                    .entry(file.new_path.clone())
                                    .or_default()
                                    .insert(text.trim().to_string());
                            }
                        }
                    }
                }
            }
        }
    }

    if files.len() > config.max_files {
        return Err(SafetyError::TooManyFiles {
            files: files.len(),
            max: config.max_files,
        });
    }

    if inserted_chars > config.max_total_inserted_chars {
        return Err(SafetyError::TooManyInsertedChars {
            chars: inserted_chars,
            max: config.max_total_inserted_chars,
        });
    }

    if config.no_new_imports {
        if let Some((file, imports)) = new_imports.into_iter().find(|(_, v)| !v.is_empty()) {
            return Err(SafetyError::NewImports {
                file,
                imports: imports.into_iter().collect(),
            });
        }
    }

    Ok(())
}

fn validate_path(path: &str, config: &PatchSafetyConfig) -> Result<(), SafetyError> {
    if path.starts_with('/') || path.starts_with('\\') || path.contains("..") {
        return Err(SafetyError::NonRelativePath {
            path: path.to_string(),
        });
    }

    if config
        .excluded_path_prefixes
        .iter()
        .any(|prefix| !prefix.is_empty() && path.starts_with(prefix))
    {
        return Err(SafetyError::ExcludedPath {
            path: path.to_string(),
        });
    }

    Ok(())
}

fn is_import_line(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with("import ") && trimmed.ends_with(';')
}

pub fn extract_new_imports(before: &str, after: &str) -> Vec<String> {
    let before_imports = import_lines(before);
    let after_imports = import_lines(after);
    after_imports
        .difference(&before_imports)
        .cloned()
        .collect()
}

fn import_lines(source: &str) -> BTreeSet<String> {
    source
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("import ") && line.ends_with(';'))
        .map(str::to_string)
        .collect()
}
