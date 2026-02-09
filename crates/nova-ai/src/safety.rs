use crate::patch::{JsonPatchOp, Patch, UnifiedDiffLine};
use crate::workspace::{AppliedPatch, VirtualWorkspace};
use globset::{Glob, GlobSet, GlobSetBuilder};
use nova_core::{LineIndex, Position as CorePosition};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct PatchSafetyConfig {
    pub max_files: usize,
    pub max_total_inserted_chars: usize,
    pub max_total_deleted_chars: usize,
    pub max_hunks_per_file: usize,
    pub max_edit_span_chars: usize,

    /// Optional allowlist of relative path prefixes that patches may touch.
    ///
    /// When empty, all (valid) relative paths are allowed.
    ///
    /// Entries ending in `/` are treated as directory prefixes (e.g. `src/`).
    /// Entries without a trailing `/` are treated as exact file paths (e.g. `src/Main.java`).
    pub allowed_path_prefixes: Vec<String>,

    /// Paths that should never be modified (simple prefix match).
    pub excluded_path_prefixes: Vec<String>,

    /// Glob patterns for excluded paths (e.g. "secret/**").
    pub excluded_path_globs: Vec<String>,

    /// If non-empty, only these extensions are allowed.
    ///
    /// Extensions include the leading dot (e.g. ".java").
    pub allowed_file_extensions: Vec<String>,

    /// Extensions that are always rejected.
    pub denied_file_extensions: Vec<String>,

    /// Whether patches are allowed to create files that are not already present in the workspace.
    ///
    /// This defaults to `false` for safety.
    pub allow_new_files: bool,

    /// Whether patches are allowed to delete existing files.
    ///
    /// This defaults to `false` for safety.
    pub allow_delete_files: bool,

    /// Whether patches are allowed to rename existing files.
    ///
    /// This defaults to `false` for safety.
    pub allow_rename_files: bool,

    pub no_new_imports: bool,
}

impl Default for PatchSafetyConfig {
    fn default() -> Self {
        Self {
            max_files: 10,
            max_total_inserted_chars: 20_000,
            max_total_deleted_chars: 20_000,
            max_hunks_per_file: 50,
            max_edit_span_chars: 20_000,
            allowed_path_prefixes: Vec::new(),
            excluded_path_prefixes: Vec::new(),
            excluded_path_globs: Vec::new(),
            allowed_file_extensions: vec![
                ".java".into(),
                ".kt".into(),
                ".gradle".into(),
                ".xml".into(),
                ".properties".into(),
                ".yml".into(),
                ".yaml".into(),
                ".md".into(),
            ],
            denied_file_extensions: Vec::new(),
            allow_new_files: false,
            allow_delete_files: false,
            allow_rename_files: false,
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
    #[error("patch deletes too many characters ({chars} > {max})")]
    TooManyDeletedChars { chars: usize, max: usize },
    #[error("patch contains too many hunks/edits for '{file}' ({hunks} > {max})")]
    TooManyHunks {
        file: String,
        hunks: usize,
        max: usize,
    },
    #[error("patch edit span for '{file}' is too large ({span} > {max})")]
    EditSpanTooLarge {
        file: String,
        span: usize,
        max: usize,
    },
    #[error("patch attempted to edit excluded path '{path}'")]
    ExcludedPath { path: String },
    #[error("patch attempted to edit path outside the allowed prefixes '{path}'")]
    NotAllowedPath { path: String },
    #[error("patch attempted to use non-relative path '{path}'")]
    NonRelativePath { path: String },
    #[error("patch attempted to edit disallowed file extension '{extension}' for '{path}'")]
    DisallowedFileExtension { path: String, extension: String },
    #[error("invalid excluded_paths glob {pattern:?}: {error}")]
    InvalidExcludedGlob { pattern: String, error: String },
    #[error("patch attempted to create a new file '{file}', but new files are not allowed")]
    NewFileNotAllowed { file: String },
    #[error("patch attempted to delete file '{file}', but file deletions are not allowed")]
    DeleteNotAllowed { file: String },
    #[error("patch attempted to rename '{from}' to '{to}', but file renames are not allowed")]
    RenameNotAllowed { from: String, to: String },
    #[error("patch introduces new imports in '{file}': {imports:?}")]
    NewImports { file: String, imports: Vec<String> },
    #[error(
        "AI patch attempted to edit outside the allowed range in '{file}' (allowed: {allowed}, touched: {touched}). \
This is a safety measure. Try running the code action again; if it persists, reselect the method body range."
    )]
    EditOutsideAllowedRange {
        file: String,
        /// Human-readable allowed range for the edit (typically 1-based line/column).
        allowed: String,
        /// Human-readable touched range (typically 1-based line/column).
        touched: String,
    },
}

pub fn enforce_patch_safety(
    patch: &Patch,
    workspace: &VirtualWorkspace,
    config: &PatchSafetyConfig,
) -> Result<(), SafetyError> {
    let excluded_globs = build_excluded_globset(config)?;
    let allowed_exts: BTreeSet<String> = config.allowed_file_extensions.iter().cloned().collect();
    let denied_exts: BTreeSet<String> = config.denied_file_extensions.iter().cloned().collect();

    let mut files = BTreeSet::new();
    let mut new_files = BTreeSet::new();
    let mut inserted_chars = 0usize;
    let mut deleted_chars = 0usize;

    match patch {
        Patch::Json(patch) => {
            let mut virtual_files: BTreeMap<String, Option<&str>> = BTreeMap::new();

            for op in &patch.ops {
                match op {
                    JsonPatchOp::Create { file, text } => {
                        validate_path(file, config, &excluded_globs, &allowed_exts, &denied_exts)?;
                        files.insert(file.clone());
                        new_files.insert(file.clone());
                        inserted_chars = inserted_chars.saturating_add(text.len());
                        let span = text.len();
                        if span > config.max_edit_span_chars {
                            return Err(SafetyError::EditSpanTooLarge {
                                file: file.clone(),
                                span,
                                max: config.max_edit_span_chars,
                            });
                        }
                        virtual_files.insert(file.clone(), Some(text));
                    }
                    JsonPatchOp::Delete { file } => {
                        if !config.allow_delete_files {
                            return Err(SafetyError::DeleteNotAllowed { file: file.clone() });
                        }
                        validate_path(file, config, &excluded_globs, &allowed_exts, &denied_exts)?;
                        files.insert(file.clone());
                        let before =
                            resolve_virtual_file(file, workspace, &virtual_files).unwrap_or("");
                        deleted_chars = deleted_chars.saturating_add(before.len());
                        let span = before.len();
                        if span > config.max_edit_span_chars {
                            return Err(SafetyError::EditSpanTooLarge {
                                file: file.clone(),
                                span,
                                max: config.max_edit_span_chars,
                            });
                        }
                        virtual_files.insert(file.clone(), None);
                    }
                    JsonPatchOp::Rename { from, to } => {
                        if !config.allow_rename_files {
                            return Err(SafetyError::RenameNotAllowed {
                                from: from.clone(),
                                to: to.clone(),
                            });
                        }
                        validate_path(from, config, &excluded_globs, &allowed_exts, &denied_exts)?;
                        validate_path(to, config, &excluded_globs, &allowed_exts, &denied_exts)?;
                        files.insert(from.clone());
                        files.insert(to.clone());

                        let before = resolve_virtual_file(from, workspace, &virtual_files);
                        virtual_files.insert(from.clone(), None);
                        virtual_files.insert(to.clone(), before);
                    }
                }
            }

            let mut edits_per_file: BTreeMap<String, usize> = BTreeMap::new();

            for edit in &patch.edits {
                validate_path(
                    &edit.file,
                    config,
                    &excluded_globs,
                    &allowed_exts,
                    &denied_exts,
                )?;
                files.insert(edit.file.clone());
                if workspace.get(&edit.file).is_none() && !virtual_files.contains_key(&edit.file) {
                    new_files.insert(edit.file.clone());
                }
                inserted_chars = inserted_chars.saturating_add(edit.text.len());

                let count = edits_per_file.entry(edit.file.clone()).or_default();
                *count += 1;
                if *count > config.max_hunks_per_file {
                    return Err(SafetyError::TooManyHunks {
                        file: edit.file.clone(),
                        hunks: *count,
                        max: config.max_hunks_per_file,
                    });
                }

                let before =
                    resolve_virtual_file(&edit.file, workspace, &virtual_files).unwrap_or("");
                let index = LineIndex::new(before);

                let start_pos =
                    CorePosition::new(edit.range.start.line, edit.range.start.character);
                let end_pos = CorePosition::new(edit.range.end.line, edit.range.end.character);

                let start = index
                    .offset_of_position(before, start_pos)
                    .map(u32::from)
                    .unwrap_or(0);
                let end = index
                    .offset_of_position(before, end_pos)
                    .map(u32::from)
                    .unwrap_or(0);
                let deleted_len = end.saturating_sub(start) as usize;
                deleted_chars = deleted_chars.saturating_add(deleted_len);

                let span = deleted_len.max(edit.text.len());
                if span > config.max_edit_span_chars {
                    return Err(SafetyError::EditSpanTooLarge {
                        file: edit.file.clone(),
                        span,
                        max: config.max_edit_span_chars,
                    });
                }
            }
        }
        Patch::UnifiedDiff(diff) => {
            for file in &diff.files {
                if file.new_path == "/dev/null" && !config.allow_delete_files {
                    return Err(SafetyError::DeleteNotAllowed {
                        file: file.old_path.clone(),
                    });
                }
                if file.old_path != "/dev/null"
                    && file.new_path != "/dev/null"
                    && file.old_path != file.new_path
                    && !config.allow_rename_files
                {
                    return Err(SafetyError::RenameNotAllowed {
                        from: file.old_path.clone(),
                        to: file.new_path.clone(),
                    });
                }

                let file_id = if file.new_path != "/dev/null" {
                    file.new_path.as_str()
                } else {
                    file.old_path.as_str()
                };

                if file.old_path != "/dev/null" {
                    validate_path(
                        &file.old_path,
                        config,
                        &excluded_globs,
                        &allowed_exts,
                        &denied_exts,
                    )?;
                    files.insert(file.old_path.clone());
                }
                if file.new_path != "/dev/null" {
                    validate_path(
                        &file.new_path,
                        config,
                        &excluded_globs,
                        &allowed_exts,
                        &denied_exts,
                    )?;
                    files.insert(file.new_path.clone());
                }
                if file.old_path == "/dev/null" && file.new_path != "/dev/null" {
                    new_files.insert(file.new_path.clone());
                }

                if file.hunks.len() > config.max_hunks_per_file {
                    return Err(SafetyError::TooManyHunks {
                        file: file_id.to_string(),
                        hunks: file.hunks.len(),
                        max: config.max_hunks_per_file,
                    });
                }

                for hunk in &file.hunks {
                    let mut old_span = 0usize;
                    let mut new_span = 0usize;
                    for line in &hunk.lines {
                        match line {
                            UnifiedDiffLine::Add(text) => {
                                inserted_chars = inserted_chars.saturating_add(text.len());
                                new_span = new_span.saturating_add(text.len() + 1);
                            }
                            UnifiedDiffLine::Remove(text) => {
                                deleted_chars = deleted_chars.saturating_add(text.len());
                                old_span = old_span.saturating_add(text.len() + 1);
                            }
                            UnifiedDiffLine::Context(text) => {
                                old_span = old_span.saturating_add(text.len() + 1);
                                new_span = new_span.saturating_add(text.len() + 1);
                            }
                        }
                    }

                    let span = old_span.max(new_span);
                    if span > config.max_edit_span_chars {
                        return Err(SafetyError::EditSpanTooLarge {
                            file: file_id.to_string(),
                            span,
                            max: config.max_edit_span_chars,
                        });
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

    if deleted_chars > config.max_total_deleted_chars {
        return Err(SafetyError::TooManyDeletedChars {
            chars: deleted_chars,
            max: config.max_total_deleted_chars,
        });
    }

    if !config.allow_new_files {
        if let Some(file) = new_files.into_iter().next() {
            return Err(SafetyError::NewFileNotAllowed { file });
        }
    }

    Ok(())
}

pub fn enforce_no_new_imports(
    before: &VirtualWorkspace,
    after: &VirtualWorkspace,
    applied: &AppliedPatch,
) -> Result<(), SafetyError> {
    for (after_path, _ranges) in &applied.touched_ranges {
        let before_path = resolve_rename_origin(after_path, &applied.renamed_files);
        let before_text = before.get(&before_path).unwrap_or("");
        let after_text = after.get(after_path).unwrap_or("");
        let imports = extract_new_imports(before_text, after_text);
        if !imports.is_empty() {
            return Err(SafetyError::NewImports {
                file: after_path.clone(),
                imports,
            });
        }
    }

    Ok(())
}

fn resolve_rename_origin(path: &str, renames: &BTreeMap<String, String>) -> String {
    let mut current = path;
    let mut visited = BTreeSet::new();

    while let Some(prev) = renames.get(current) {
        if !visited.insert(current.to_string()) {
            break;
        }
        current = prev;
    }

    current.to_string()
}

fn build_excluded_globset(config: &PatchSafetyConfig) -> Result<Option<GlobSet>, SafetyError> {
    if config.excluded_path_globs.is_empty() {
        return Ok(None);
    }

    let mut builder = GlobSetBuilder::new();
    for pattern in &config.excluded_path_globs {
        let glob = Glob::new(pattern).map_err(|err| SafetyError::InvalidExcludedGlob {
            pattern: pattern.clone(),
            error: err.to_string(),
        })?;
        builder.add(glob);
    }

    let set = builder
        .build()
        .map_err(|err| SafetyError::InvalidExcludedGlob {
            pattern: "<globset build>".into(),
            error: err.to_string(),
        })?;

    Ok(Some(set))
}

fn validate_path(
    path: &str,
    config: &PatchSafetyConfig,
    excluded_globs: &Option<GlobSet>,
    allowed_exts: &BTreeSet<String>,
    denied_exts: &BTreeSet<String>,
) -> Result<(), SafetyError> {
    let path = normalize_patch_path(path);
    if path.starts_with('/') || path.starts_with('\\') || path.contains('\\') {
        return Err(SafetyError::NonRelativePath {
            path: path.to_string(),
        });
    }

    // Disallow traversal and drive letters / URI schemes.
    //
    // We also reject empty (`//`) and current-directory (`./`) segments to avoid
    // bypassing prefix/glob allowlists with non-canonical paths.
    if path.contains(':')
        || path
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(SafetyError::NonRelativePath {
            path: path.to_string(),
        });
    }

    if config
        .allowed_path_prefixes
        .iter()
        .any(|prefix| !prefix.is_empty())
        && !config
            .allowed_path_prefixes
            .iter()
            .filter(|prefix| !prefix.is_empty())
            .any(|prefix| path_matches_prefix(path, prefix))
    {
        return Err(SafetyError::NotAllowedPath {
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

    if let Some(set) = excluded_globs {
        if set.is_match(Path::new(path)) {
            return Err(SafetyError::ExcludedPath {
                path: path.to_string(),
            });
        }
    }

    let ext = Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| format!(".{ext}"))
        .unwrap_or_else(|| "<none>".to_string());

    if denied_exts.contains(&ext) {
        return Err(SafetyError::DisallowedFileExtension {
            path: path.to_string(),
            extension: ext,
        });
    }

    if !allowed_exts.is_empty() && !allowed_exts.contains(&ext) {
        return Err(SafetyError::DisallowedFileExtension {
            path: path.to_string(),
            extension: ext,
        });
    }

    Ok(())
}

fn normalize_patch_path(path: &str) -> &str {
    // Patches are expected to already use forward slashes (`/`) as separators. We do not attempt
    // to normalize Windows backslashes (`\`) because failing closed is safer than trying to guess
    // intent (e.g. UNC paths like `\\server\\share` or device paths like `\\\\?\\C:\\...`).
    path
}

fn path_matches_prefix(path: &str, prefix: &str) -> bool {
    if prefix.ends_with('/') {
        // Treat an explicit trailing slash as "directory prefix".
        path.starts_with(prefix)
    } else {
        // Otherwise, treat it as an exact file path.
        path == prefix
    }
}

fn resolve_virtual_file<'a>(
    path: &str,
    workspace: &'a VirtualWorkspace,
    virtual_files: &BTreeMap<String, Option<&'a str>>,
) -> Option<&'a str> {
    match virtual_files.get(path) {
        Some(Some(text)) => Some(*text),
        Some(None) => None,
        None => workspace.get(path),
    }
}

pub fn extract_new_imports(before: &str, after: &str) -> Vec<String> {
    let before_imports = import_lines(before);
    let after_imports = import_lines(after);
    after_imports.difference(&before_imports).cloned().collect()
}

fn import_lines(source: &str) -> BTreeSet<String> {
    source
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("import ") && line.ends_with(';'))
        .map(str::to_string)
        .collect()
}
