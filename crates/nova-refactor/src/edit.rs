//! Canonical edit model used by Nova refactorings.
//!
//! The key type in this module is [`WorkspaceEdit`], which represents a deterministic, composable
//! set of file operations (rename/create/delete) plus byte-range text edits.
//!
//! ## Why a custom edit model?
//!
//! - Refactorings frequently need to rename/move files *and* update their contents.
//! - LSP's edit model has multiple encodings (`changes` vs `documentChanges`) and uses UTF-16
//!   positions, while refactorings naturally operate in byte offsets.
//! - Many refactorings are easier to compose and preview if we converge on a single canonical
//!   representation with well-defined invariants.
//!
//! The invariants and normalization logic are documented on [`WorkspaceEdit`].

use std::collections::{BTreeMap, BTreeSet};

pub use nova_index::TextRange;
use thiserror::Error;

/// Identifier for a workspace file.
///
/// This is the key used by [`WorkspaceEdit`] to refer to files.
///
/// In a real Nova implementation this would likely be an interned ID or a URI. In this refactoring
/// crate it is a plain string to keep tests lightweight.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileId(pub String);

/// Canonical file identifier type for refactorings.
///
/// Today this is the same as [`FileId`]. The alias exists to make it clear which `FileId` is
/// intended for workspace edits (vs. `nova-vfs` / semantic ids).
pub type RefactorFileId = FileId;

impl FileId {
    pub fn new(path: impl Into<String>) -> Self {
        Self(path.into())
    }
}

/// A byte-range edit within a single file.
///
/// This is the canonical text edit used by [`WorkspaceEdit`]. At the crate root it is re-exported
/// as [`crate::WorkspaceTextEdit`] (to avoid colliding with the legacy
/// `safe_delete::TextEdit` type).
///
/// ## Offsets and ranges
///
/// - `range` uses **byte offsets** into the file's UTF-8 text.
/// - The range is half-open: `[start, end)`.
/// - An insert is represented by a zero-length range (`start == end`).
/// - A delete is represented by an empty replacement string (`replacement.is_empty()`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextEdit {
    pub file: FileId,
    pub range: TextRange,
    pub replacement: String,
}

impl TextEdit {
    pub fn insert(file: FileId, offset: usize, text: impl Into<String>) -> Self {
        Self {
            file,
            range: TextRange::new(offset, offset),
            replacement: text.into(),
        }
    }

    pub fn replace(file: FileId, range: TextRange, text: impl Into<String>) -> Self {
        Self {
            file,
            range,
            replacement: text.into(),
        }
    }

    pub fn delete(file: FileId, range: TextRange) -> Self {
        Self {
            file,
            range,
            replacement: String::new(),
        }
    }
}

/// File-level operations supported by Nova refactorings.
///
/// [`WorkspaceEdit`] applies file operations **before** applying any [`TextEdit`]s.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileOp {
    /// Rename/move a workspace file.
    Rename { from: FileId, to: FileId },
    /// Create a new file with initial contents.
    Create { file: FileId, contents: String },
    /// Delete a file.
    Delete { file: FileId },
}

/// A deterministic set of changes across potentially multiple files.
///
/// A [`WorkspaceEdit`] is Nova's canonical refactoring output. It is intentionally more constrained
/// than LSP's edit model so that refactorings can be composed, previewed, and converted to LSP
/// predictably.
///
/// ## Semantics
///
/// Applying a workspace edit happens in two phases:
///
/// 1. Apply [`FileOp`]s in order (rename/create/delete).
/// 2. Apply [`TextEdit`]s grouped by file.
///
/// This means that the `file` field of each [`TextEdit`] is interpreted in the *post-file-op*
/// workspace.
///
/// ## Normalization and invariants
///
/// Many consumers (preview generation, LSP conversion, in-memory application) require a normalized
/// edit. [`WorkspaceEdit::normalize`] enforces the following invariants:
///
/// ### Text edit invariants
///
/// - **Deterministic ordering:** `text_edits` are sorted by `(file, range.start, range.end,
///   replacement)`.
/// - **Deduplication:** exact duplicates are removed.
/// - **Non-overlap:** within each file, ranges must not overlap (`next.start < prev.end` is an
///   error). Touching ranges (`next.start == prev.end`) are allowed.
/// - **Insert merging:** multiple inserts at the same position are merged into a single insert by
///   concatenating their `replacement` strings in sorted order. This avoids ambiguous "which insert
///   comes first?" behavior.
/// - **Valid ranges:** `range.start <= range.end`.
///
/// ### File op invariants and ordering
///
/// - Duplicate file ops are deduplicated where safe (e.g. repeated deletes).
/// - Conflicting ops (create+delete, rename collisions, etc.) are rejected.
/// - File operations are ordered deterministically:
///   1. **Renames** first, in a topological order that is safe to apply sequentially.
///      For example, a chain `A -> B, B -> C` is ordered as `B -> C` then `A -> B`.
///   2. **Creates** next, sorted by file id.
///   3. **Deletes** last, sorted by file id.
///
/// Deleting last is a conservative choice: if a later create/rename fails in the client, we avoid
/// having already removed the original file.
///
/// ### Post-rename file ids
///
/// Because file operations are applied before text edits, text edits must target the **post-rename
/// file ids**.
///
/// Concretely, if a refactoring renames `old.java` to `new.java`, then any content edits for that
/// file must use `file = new.java`. If an edit incorrectly targets `old.java`,
/// [`WorkspaceEdit::normalize`] returns [`EditError::TextEditTargetsRenamedFile`].
///
/// A rename chain is not transitive in terms of "where does the original file end up?". For
/// example, `A -> B` and `B -> C` results in `A` ending up at `B` and `B` ending up at `C`. This is
/// why Nova treats each rename as moving the *current* contents of `from` to `to` and why
/// [`WorkspaceEdit::remap_text_edits_across_renames`] performs a **direct** (non-transitive) remap.
///
/// ## Producing edits: emitting vs. remapping
///
/// There are two correct ways to produce a workspace edit involving renames:
///
/// 1. **Preferred:** emit text edits directly against the post-rename ids.
/// 2. If you naturally produce edits against pre-rename ids, add the [`FileOp::Rename`] operations
///    and then call [`WorkspaceEdit::remap_text_edits_across_renames`] **before**
///    [`WorkspaceEdit::normalize`].
///
/// If you call `normalize()` first while still targeting pre-rename ids, it will fail.
///
/// ## Relationship to LSP conversion
///
/// This crate provides two conversions:
///
/// - [`crate::workspace_edit_to_lsp`] uses the LSP `changes` map. It **cannot** represent file
///   operations and returns an error if `file_ops` is non-empty.
/// - [`crate::workspace_edit_to_lsp_document_changes`] uses LSP `documentChanges` and can represent
///   renames/creates/deletes.
///
/// Both conversions clone and normalize the edit internally, so they rely on the invariants above
/// (especially "text edits target post-rename ids").
///
/// ## Example: rename a file and edit its contents
///
/// ```
/// use std::collections::BTreeMap;
///
/// use nova_refactor::{
///     apply_workspace_edit, FileId, FileOp, WorkspaceEdit, WorkspaceTextEdit, WorkspaceTextRange,
/// };
///
/// // Start with a single file.
/// let old = FileId::new("file:///A.java");
/// let new = FileId::new("file:///B.java");
/// let mut files = BTreeMap::new();
/// files.insert(old.clone(), "class A {}".to_string());
///
/// // Rename the file and update its contents.
/// let mut edit = WorkspaceEdit {
///     file_ops: vec![FileOp::Rename {
///         from: old.clone(),
///         to: new.clone(),
///     }],
///     // Note: the text edit targets the *post-rename* file id (`new`), not `old`.
///     text_edits: vec![WorkspaceTextEdit::replace(
///         new.clone(),
///         WorkspaceTextRange::new(6, 7),
///         "B",
///     )],
/// };
/// edit.normalize().unwrap();
///
/// let out = apply_workspace_edit(&files, &edit).unwrap();
/// assert_eq!(out.get(&new).unwrap(), "class B {}");
/// assert!(!out.contains_key(&old));
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkspaceEdit {
    pub file_ops: Vec<FileOp>,
    pub text_edits: Vec<TextEdit>,
}

impl WorkspaceEdit {
    pub fn new(edits: Vec<TextEdit>) -> Self {
        Self {
            file_ops: Vec::new(),
            text_edits: edits,
        }
    }

    /// Merge two [`WorkspaceEdit`] values into a single edit.
    ///
    /// This is the supported way to *compose* refactorings from independent primitives.
    ///
    /// The merged edit is always [`normalize`](WorkspaceEdit::normalize)d, so:
    /// - text edits are sorted/deduplicated and validated to be non-overlapping
    /// - multiple inserts at the same position are merged deterministically
    /// - file operations are de-duplicated, validated for collisions, and ordered deterministically
    ///
    /// # File ids and renames ("coordinate systems")
    ///
    /// Text edits in a [`WorkspaceEdit`] are interpreted in the file-id "coordinate system"
    /// *after* applying the edit's own [`file_ops`](WorkspaceEdit::file_ops). In particular, if an
    /// edit renames `a` → `b`, then any text edits intended for that file must target `b` (not
    /// `a`).
    ///
    /// When composing edits that include renames, this method assumes that *both inputs already
    /// follow that rule*. If a producer emitted text edits against pre-rename file ids, it must
    /// call [`remap_text_edits_across_renames`](WorkspaceEdit::remap_text_edits_across_renames) (or
    /// otherwise remap its file ids) *before* merging.
    pub fn merge(mut self, other: WorkspaceEdit) -> Result<Self, EditError> {
        self.file_ops.extend(other.file_ops);
        self.text_edits.extend(other.text_edits);
        self.normalize()?;
        Ok(self)
    }

    pub fn is_empty(&self) -> bool {
        self.file_ops.is_empty() && self.text_edits.is_empty()
    }

    /// Returns edits grouped by file in deterministic order.
    pub fn edits_by_file(&self) -> BTreeMap<&FileId, Vec<&TextEdit>> {
        let mut map: BTreeMap<&FileId, Vec<&TextEdit>> = BTreeMap::new();
        for edit in &self.text_edits {
            map.entry(&edit.file).or_default().push(edit);
        }
        for edits in map.values_mut() {
            edits.sort_by(|a, b| {
                a.range
                    .start
                    .cmp(&b.range.start)
                    .then_with(|| a.range.end.cmp(&b.range.end))
                    .then_with(|| a.replacement.cmp(&b.replacement))
            });
        }
        map
    }

    /// Normalize edits (sort, deduplicate, and validate non-overlap).
    ///
    /// Most consumers call `normalize()` internally (e.g. preview generation and LSP conversion),
    /// but producers may also call it to:
    ///
    /// - validate invariants early and return a structured [`EditError`]
    /// - ensure deterministic ordering for snapshot tests / stable output
    ///
    /// See [`WorkspaceEdit`] for the full list of invariants.
    pub fn normalize(&mut self) -> Result<(), EditError> {
        self.normalize_file_ops()?;
        self.normalize_text_edits()?;
        self.validate_text_edits_target_post_file_ops()?;
        Ok(())
    }

    /// Remap all text edits by applying the rename mapping in `file_ops`.
    ///
    /// This is a convenience for producers that generated edits against the pre-rename file ids.
    /// Callers that already emit edits against post-rename file ids should *not* call this.
    ///
    /// ### When to use this
    ///
    /// If you have a [`WorkspaceEdit`] that contains [`FileOp::Rename`] operations *and* you
    /// generated `text_edits` targeting the old ids, call this method **before**
    /// [`WorkspaceEdit::normalize`]. Normalization will otherwise return
    /// [`EditError::TextEditTargetsRenamedFile`].
    ///
    /// ### Direct (non-transitive) mapping
    ///
    /// This method applies only the direct `from -> to` mapping from each rename operation; it does
    /// not transitively follow rename chains. This matches the semantics of applying renames
    /// sequentially (topologically ordered) and is important for chains like `A -> B, B -> C`, where
    /// the original `A` ends up at `B`, not `C`.
    pub fn remap_text_edits_across_renames(&mut self) -> Result<(), EditError> {
        let mapping = self.rename_mapping()?;
        for edit in &mut self.text_edits {
            if let Some(to) = mapping.get(&edit.file) {
                edit.file = to.clone();
            }
        }
        Ok(())
    }

    fn normalize_file_ops(&mut self) -> Result<(), EditError> {
        if self.file_ops.is_empty() {
            return Ok(());
        }

        // Collect operations into canonical maps/sets so we can de-duplicate and validate.
        let mut deletes: BTreeSet<FileId> = BTreeSet::new();
        let mut creates: BTreeMap<FileId, String> = BTreeMap::new();
        let mut renames: BTreeMap<FileId, FileId> = BTreeMap::new();
        let mut rename_targets: BTreeMap<FileId, FileId> = BTreeMap::new(); // to -> from

        for op in self.file_ops.drain(..) {
            match op {
                FileOp::Delete { file } => {
                    deletes.insert(file);
                }
                FileOp::Create { file, contents } => {
                    if let Some(prev) = creates.get(&file) {
                        if prev != &contents {
                            return Err(EditError::DuplicateCreate { file });
                        }
                        continue;
                    }
                    creates.insert(file, contents);
                }
                FileOp::Rename { from, to } => {
                    if from == to {
                        return Err(EditError::InvalidRename { from, to });
                    }

                    if let Some(prev_to) = renames.get(&from) {
                        if prev_to != &to {
                            return Err(EditError::DuplicateRenameSource {
                                from,
                                first_to: prev_to.clone(),
                                second_to: to,
                            });
                        }
                        continue;
                    }

                    if let Some(prev_from) = rename_targets.get(&to) {
                        if prev_from != &from {
                            return Err(EditError::DuplicateRenameDestination {
                                to,
                                first_from: prev_from.clone(),
                                second_from: from,
                            });
                        }
                        continue;
                    }

                    rename_targets.insert(to.clone(), from.clone());
                    renames.insert(from, to);
                }
            }
        }

        // Validate file op collisions (keep it conservative for now).
        for file in deletes.iter() {
            if creates.contains_key(file) {
                return Err(EditError::CreateDeleteConflict { file: file.clone() });
            }
            if renames.contains_key(file) || rename_targets.contains_key(file) {
                return Err(EditError::FileOpCollision {
                    file: file.clone(),
                    op: "delete",
                });
            }
        }

        for file in creates.keys() {
            if renames.contains_key(file) || rename_targets.contains_key(file) {
                return Err(EditError::FileOpCollision {
                    file: file.clone(),
                    op: "create",
                });
            }
        }

        let renames = order_renames(&renames)?;

        // Deterministic ordering: renames (topologically ordered), creates, deletes.
        //
        // Putting deletes last is safer for clients that apply the operations sequentially:
        // if a later create/rename fails, we avoid having already removed the original file.
        self.file_ops = renames
            .into_iter()
            .chain(
                creates
                    .into_iter()
                    .map(|(file, contents)| FileOp::Create { file, contents }),
            )
            .chain(deletes.into_iter().map(|file| FileOp::Delete { file }))
            .collect();

        Ok(())
    }

    fn normalize_text_edits(&mut self) -> Result<(), EditError> {
        self.text_edits.sort_by(|a, b| {
            a.file
                .cmp(&b.file)
                .then_with(|| a.range.start.cmp(&b.range.start))
                .then_with(|| a.range.end.cmp(&b.range.end))
                .then_with(|| a.replacement.cmp(&b.replacement))
        });

        // Exact duplicates are redundant.
        self.text_edits.dedup_by(|a, b| {
            a.file == b.file && a.range == b.range && a.replacement == b.replacement
        });

        // Merge multiple inserts at the same position. This avoids ambiguous ordering when applying
        // edits and keeps the edit set deterministic.
        let mut merged: Vec<TextEdit> = Vec::with_capacity(self.text_edits.len());
        for edit in self.text_edits.drain(..) {
            if let Some(last) = merged.last_mut() {
                if last.file == edit.file
                    && last.range == edit.range
                    && last.range.start == last.range.end
                {
                    last.replacement.push_str(&edit.replacement);
                    continue;
                }

                if last.file == edit.file
                    && last.range == edit.range
                    && last.replacement != edit.replacement
                {
                    return Err(EditError::OverlappingEdits {
                        file: edit.file,
                        first: last.range,
                        second: edit.range,
                    });
                }
            }
            merged.push(edit);
        }

        self.text_edits = merged;

        // Validate non-overlap per file.
        let mut current_file: Option<&FileId> = None;
        let mut prev: Option<TextRange> = None;
        for edit in &self.text_edits {
            if edit.range.start > edit.range.end {
                return Err(EditError::InvalidRange {
                    file: edit.file.clone(),
                    range: edit.range,
                });
            }

            if current_file.map(|f| f != &edit.file).unwrap_or(true) {
                current_file = Some(&edit.file);
                prev = None;
            }

            if let Some(prev_range) = prev {
                if edit.range.start < prev_range.end {
                    return Err(EditError::OverlappingEdits {
                        file: edit.file.clone(),
                        first: prev_range,
                        second: edit.range,
                    });
                }
            }

            prev = Some(edit.range);
        }

        Ok(())
    }

    fn rename_mapping(&self) -> Result<BTreeMap<FileId, FileId>, EditError> {
        let mut mapping: BTreeMap<FileId, FileId> = BTreeMap::new();
        for op in &self.file_ops {
            let FileOp::Rename { from, to } = op else {
                continue;
            };
            if let Some(prev) = mapping.get(from) {
                if prev != to {
                    return Err(EditError::DuplicateRenameSource {
                        from: from.clone(),
                        first_to: prev.clone(),
                        second_to: to.clone(),
                    });
                }
            } else {
                mapping.insert(from.clone(), to.clone());
            }
        }
        // Validate there are no cycles so `remap_text_edits_across_renames` can be used before
        // `normalize()`.
        let _ = order_renames(&mapping)?;
        Ok(mapping)
    }

    fn validate_text_edits_target_post_file_ops(&self) -> Result<(), EditError> {
        if self.file_ops.is_empty() || self.text_edits.is_empty() {
            return Ok(());
        }

        // Conservative invariants:
        // - Text edits should not target deleted files.
        // - Text edits should not target files that are renamed away *and have no incoming rename*
        //   (i.e. sources that are not also destinations).
        let mut deleted: BTreeSet<&FileId> = BTreeSet::new();
        let mut rename_sources: BTreeSet<&FileId> = BTreeSet::new();
        let mut rename_dests: BTreeSet<&FileId> = BTreeSet::new();

        for op in &self.file_ops {
            match op {
                FileOp::Delete { file } => {
                    deleted.insert(file);
                }
                FileOp::Rename { from, to } => {
                    rename_sources.insert(from);
                    rename_dests.insert(to);
                }
                FileOp::Create { .. } => {}
            }
        }

        for edit in &self.text_edits {
            if deleted.contains(&edit.file) {
                return Err(EditError::TextEditTargetsDeletedFile {
                    file: edit.file.clone(),
                });
            }

            if rename_sources.contains(&edit.file) && !rename_dests.contains(&edit.file) {
                let mapping = self.rename_mapping()?;
                let renamed_to = mapping
                    .get(&edit.file)
                    .expect("rename mapping must contain rename source")
                    .clone();
                return Err(EditError::TextEditTargetsRenamedFile {
                    file: edit.file.clone(),
                    renamed_to,
                });
            }
        }

        Ok(())
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum EditError {
    #[error("invalid text range {range:?} in {file:?}")]
    InvalidRange { file: FileId, range: TextRange },
    #[error("overlapping edits in {file:?}: {first:?} overlaps {second:?}")]
    OverlappingEdits {
        file: FileId,
        first: TextRange,
        second: TextRange,
    },
    #[error("text edit range {range:?} is outside the file bounds (len={len}) in {file:?}")]
    OutOfBounds {
        file: FileId,
        range: TextRange,
        len: usize,
    },
    #[error("offset {offset} is not a UTF-8 character boundary in {file:?}")]
    InvalidUtf8Boundary { file: FileId, offset: usize },
    #[error("unknown file {0:?}")]
    UnknownFile(FileId),
    #[error("file already exists {0:?}")]
    FileAlreadyExists(FileId),
    #[error("invalid rename operation {from:?} -> {to:?}")]
    InvalidRename { from: FileId, to: FileId },
    #[error("multiple create operations for {file:?} with different contents")]
    DuplicateCreate { file: FileId },
    #[error("multiple rename operations for {from:?}: {first_to:?} and {second_to:?}")]
    DuplicateRenameSource {
        from: FileId,
        first_to: FileId,
        second_to: FileId,
    },
    #[error("multiple files renamed to {to:?}: {first_from:?} and {second_from:?}")]
    DuplicateRenameDestination {
        to: FileId,
        first_from: FileId,
        second_from: FileId,
    },
    #[error("rename cycle detected involving {file:?}")]
    RenameCycle { file: FileId },
    #[error("file {file:?} is both created and deleted")]
    CreateDeleteConflict { file: FileId },
    #[error("file {file:?} has a conflicting file operation ({op})")]
    FileOpCollision { file: FileId, op: &'static str },
    #[error("text edits must target post-rename file ids; {file:?} is renamed to {renamed_to:?}")]
    TextEditTargetsRenamedFile { file: FileId, renamed_to: FileId },
    #[error("text edit targets deleted file {file:?}")]
    TextEditTargetsDeletedFile { file: FileId },
}

/// Apply a set of edits to `original` and return the modified text.
///
/// The input edits must be non-overlapping and valid for the `original` text.
pub fn apply_text_edits(original: &str, edits: &[TextEdit]) -> Result<String, EditError> {
    if edits.is_empty() {
        return Ok(original.to_string());
    }

    let mut sorted = edits.to_vec();
    sorted.sort_by(|a, b| {
        b.range
            .start
            .cmp(&a.range.start)
            .then_with(|| b.range.end.cmp(&a.range.end))
            .then_with(|| b.replacement.cmp(&a.replacement))
    });

    let mut out = original.to_string();
    for edit in sorted {
        let len = out.len();
        if edit.range.end > len || edit.range.start > edit.range.end {
            return Err(EditError::OutOfBounds {
                file: edit.file,
                range: edit.range,
                len,
            });
        }

        if !out.is_char_boundary(edit.range.start) {
            return Err(EditError::InvalidUtf8Boundary {
                file: edit.file,
                offset: edit.range.start,
            });
        }
        if !out.is_char_boundary(edit.range.end) {
            return Err(EditError::InvalidUtf8Boundary {
                file: edit.file,
                offset: edit.range.end,
            });
        }

        out.replace_range(edit.range.start..edit.range.end, &edit.replacement);
    }

    Ok(out)
}

/// Apply a workspace edit (file operations, then text edits) to an in-memory workspace.
pub fn apply_workspace_edit(
    files: &BTreeMap<FileId, String>,
    edit: &WorkspaceEdit,
) -> Result<BTreeMap<FileId, String>, EditError> {
    let mut normalized = edit.clone();
    normalized.normalize()?;

    let mut out = files.clone();
    apply_file_ops_in_place(&mut out, &normalized.file_ops)?;

    // Group by file and apply text edits from end to start for stable offsets.
    let mut grouped: BTreeMap<FileId, Vec<TextEdit>> = BTreeMap::new();
    for e in &normalized.text_edits {
        grouped.entry(e.file.clone()).or_default().push(e.clone());
    }

    for (file, edits) in grouped {
        let Some(text) = out.get(&file).cloned() else {
            return Err(EditError::UnknownFile(file));
        };
        let updated = apply_text_edits(&text, &edits)?;
        out.insert(file, updated);
    }

    Ok(out)
}

fn apply_file_ops_in_place(
    files: &mut BTreeMap<FileId, String>,
    ops: &[FileOp],
) -> Result<(), EditError> {
    for op in ops {
        match op {
            FileOp::Delete { file } => {
                let removed = files.remove(file);
                if removed.is_none() {
                    return Err(EditError::UnknownFile(file.clone()));
                }
            }
            FileOp::Rename { from, to } => {
                if files.contains_key(to) {
                    return Err(EditError::FileAlreadyExists(to.clone()));
                }
                let Some(contents) = files.remove(from) else {
                    return Err(EditError::UnknownFile(from.clone()));
                };
                files.insert(to.clone(), contents);
            }
            FileOp::Create { file, contents } => {
                if files.contains_key(file) {
                    return Err(EditError::FileAlreadyExists(file.clone()));
                }
                files.insert(file.clone(), contents.clone());
            }
        }
    }
    Ok(())
}

fn order_renames(renames: &BTreeMap<FileId, FileId>) -> Result<Vec<FileOp>, EditError> {
    let mut visiting: BTreeSet<FileId> = BTreeSet::new();
    let mut visited: BTreeSet<FileId> = BTreeSet::new();
    let mut ordered: Vec<FileOp> = Vec::with_capacity(renames.len());

    fn visit(
        from: &FileId,
        renames: &BTreeMap<FileId, FileId>,
        visiting: &mut BTreeSet<FileId>,
        visited: &mut BTreeSet<FileId>,
        ordered: &mut Vec<FileOp>,
    ) -> Result<(), EditError> {
        if visited.contains(from) {
            return Ok(());
        }
        if !visiting.insert(from.clone()) {
            return Err(EditError::RenameCycle { file: from.clone() });
        }

        let to = renames.get(from).expect("from is present in map");
        if renames.contains_key(to) {
            visit(to, renames, visiting, visited, ordered)?;
        }

        visiting.remove(from);
        visited.insert(from.clone());
        ordered.push(FileOp::Rename {
            from: from.clone(),
            to: to.clone(),
        });
        Ok(())
    }

    for from in renames.keys() {
        visit(from, renames, &mut visiting, &mut visited, &mut ordered)?;
    }

    Ok(ordered)
}

impl From<crate::safe_delete::TextEdit> for TextEdit {
    fn from(edit: crate::safe_delete::TextEdit) -> Self {
        Self {
            file: FileId::new(edit.file),
            range: edit.range,
            replacement: edit.replacement,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn normalize_dedups_and_merges_inserts_deterministically() {
        let file = FileId::new("file:///test");
        let mut edit = WorkspaceEdit::new(vec![
            TextEdit::insert(file.clone(), 0, "b"),
            TextEdit::insert(file.clone(), 0, "a"),
            TextEdit::insert(file.clone(), 0, "a"),
        ]);

        edit.normalize().unwrap();
        assert_eq!(edit.text_edits, vec![TextEdit::insert(file, 0, "ab")]);
    }

    #[test]
    fn normalize_detects_overlapping_edits() {
        let file = FileId::new("file:///test");
        let mut edit = WorkspaceEdit::new(vec![
            TextEdit::replace(file.clone(), TextRange::new(0, 2), "x"),
            TextEdit::replace(file.clone(), TextRange::new(1, 3), "y"),
        ]);

        let err = edit.normalize().unwrap_err();
        assert!(matches!(err, EditError::OverlappingEdits { .. }));
    }

    #[test]
    fn apply_orders_renames_before_text_edits() {
        let a = FileId::new("file:///a");
        let b = FileId::new("file:///b");
        let c = FileId::new("file:///c");

        let mut files = BTreeMap::new();
        files.insert(a.clone(), "A".to_string());
        files.insert(b.clone(), "B".to_string());

        // Simulate A->B and B->C chain. Must apply B->C first, then A->B.
        let mut edit = WorkspaceEdit {
            file_ops: vec![
                FileOp::Rename {
                    from: a.clone(),
                    to: b.clone(),
                },
                FileOp::Rename {
                    from: b.clone(),
                    to: c.clone(),
                },
            ],
            text_edits: vec![TextEdit::replace(b.clone(), TextRange::new(0, 1), "X")],
        };
        edit.normalize().unwrap();

        let out = apply_workspace_edit(&files, &edit).unwrap();
        assert_eq!(out.get(&b).map(String::as_str), Some("X"));
        assert_eq!(out.get(&c).map(String::as_str), Some("B"));
        assert!(!out.contains_key(&a));
    }

    #[test]
    fn apply_creates_files_before_applying_text_edits() {
        let created = FileId::new("file:///created");
        let files = BTreeMap::new();

        let edit = WorkspaceEdit {
            file_ops: vec![FileOp::Create {
                file: created.clone(),
                contents: "hi".to_string(),
            }],
            text_edits: vec![TextEdit::insert(created.clone(), 2, "!")],
        };

        let out = apply_workspace_edit(&files, &edit).unwrap();
        assert_eq!(out.get(&created).map(String::as_str), Some("hi!"));
    }

    #[test]
    fn normalize_rename_source_error_reports_direct_destination() {
        let a = FileId::new("file:///a");
        let b = FileId::new("file:///b");
        let c = FileId::new("file:///c");

        let mut edit = WorkspaceEdit {
            file_ops: vec![
                FileOp::Rename {
                    from: a.clone(),
                    to: b.clone(),
                },
                FileOp::Rename {
                    from: b.clone(),
                    to: c.clone(),
                },
            ],
            text_edits: vec![TextEdit::insert(a.clone(), 0, "x")],
        };

        let err = edit.normalize().unwrap_err();
        match err {
            EditError::TextEditTargetsRenamedFile { file, renamed_to } => {
                assert_eq!(file, a);
                assert_eq!(renamed_to, b);
            }
            other => panic!("expected TextEditTargetsRenamedFile, got {other:?}"),
        }
    }

    #[test]
    fn remap_text_edits_maps_sources_directly_not_transitively() {
        let a = FileId::new("file:///a");
        let b = FileId::new("file:///b");
        let c = FileId::new("file:///c");

        let mut edit = WorkspaceEdit {
            file_ops: vec![
                FileOp::Rename {
                    from: a.clone(),
                    to: b.clone(),
                },
                FileOp::Rename {
                    from: b.clone(),
                    to: c.clone(),
                },
            ],
            text_edits: vec![TextEdit::insert(a.clone(), 0, "x")],
        };

        edit.remap_text_edits_across_renames().unwrap();
        assert_eq!(edit.text_edits[0].file, b);
    }

    #[test]
    fn normalize_orders_file_ops_renames_creates_deletes() {
        let a = FileId::new("file:///a");
        let a2 = FileId::new("file:///a2");
        let b = FileId::new("file:///b");
        let c = FileId::new("file:///c");

        let mut edit = WorkspaceEdit {
            file_ops: vec![
                FileOp::Delete { file: c.clone() },
                FileOp::Create {
                    file: b.clone(),
                    contents: "b".to_string(),
                },
                FileOp::Rename {
                    from: a.clone(),
                    to: a2.clone(),
                },
            ],
            text_edits: Vec::new(),
        };

        edit.normalize().unwrap();

        assert!(matches!(edit.file_ops.get(0), Some(FileOp::Rename { .. })));
        assert!(matches!(edit.file_ops.get(1), Some(FileOp::Create { .. })));
        assert!(matches!(edit.file_ops.get(2), Some(FileOp::Delete { .. })));
    }

    #[test]
    fn merge_disjoint_edits_succeeds_and_normalizes_ordering() {
        let a = FileId::new("file:///a");
        let b = FileId::new("file:///b");
        let c = FileId::new("file:///c");

        // Note: the ops and text edits are intentionally in "non-normal" ordering to ensure merge
        // calls `normalize()`.
        let edit1 = WorkspaceEdit {
            file_ops: vec![FileOp::Delete { file: c.clone() }],
            text_edits: vec![TextEdit::insert(b.clone(), 0, "b")],
        };
        let edit2 = WorkspaceEdit {
            file_ops: vec![FileOp::Create {
                file: a.clone(),
                contents: "A".to_string(),
            }],
            text_edits: vec![TextEdit::insert(a.clone(), 0, "a")],
        };

        let merged = edit1.merge(edit2).unwrap();

        assert_eq!(
            merged.file_ops,
            vec![
                FileOp::Create {
                    file: a.clone(),
                    contents: "A".to_string()
                },
                FileOp::Delete { file: c }
            ]
        );
        assert_eq!(
            merged.text_edits,
            vec![TextEdit::insert(a, 0, "a"), TextEdit::insert(b, 0, "b")]
        );
    }

    #[test]
    fn merge_overlapping_text_edits_fails() {
        let file = FileId::new("file:///test");
        let edit1 = WorkspaceEdit::new(vec![TextEdit::replace(
            file.clone(),
            TextRange::new(0, 2),
            "x",
        )]);
        let edit2 = WorkspaceEdit::new(vec![TextEdit::replace(
            file.clone(),
            TextRange::new(1, 3),
            "y",
        )]);

        let err = edit1.merge(edit2).unwrap_err();
        assert!(matches!(err, EditError::OverlappingEdits { .. }));
    }

    #[test]
    fn merge_conflicting_file_ops_duplicate_rename_source_fails() {
        let a = FileId::new("file:///a");
        let b = FileId::new("file:///b");
        let c = FileId::new("file:///c");

        let edit1 = WorkspaceEdit {
            file_ops: vec![FileOp::Rename {
                from: a.clone(),
                to: b,
            }],
            text_edits: Vec::new(),
        };
        let edit2 = WorkspaceEdit {
            file_ops: vec![FileOp::Rename { from: a, to: c }],
            text_edits: Vec::new(),
        };

        let err = edit1.merge(edit2).unwrap_err();
        assert!(matches!(err, EditError::DuplicateRenameSource { .. }));
    }

    #[test]
    fn merge_conflicting_file_ops_duplicate_rename_destination_fails() {
        let a = FileId::new("file:///a");
        let b = FileId::new("file:///b");
        let c = FileId::new("file:///c");

        let edit1 = WorkspaceEdit {
            file_ops: vec![FileOp::Rename {
                from: a,
                to: c.clone(),
            }],
            text_edits: Vec::new(),
        };
        let edit2 = WorkspaceEdit {
            file_ops: vec![FileOp::Rename { from: b, to: c }],
            text_edits: Vec::new(),
        };

        let err = edit1.merge(edit2).unwrap_err();
        assert!(matches!(err, EditError::DuplicateRenameDestination { .. }));
    }

    #[test]
    fn merge_conflicting_file_ops_collision_fails() {
        let a = FileId::new("file:///a");
        let b = FileId::new("file:///b");

        let edit1 = WorkspaceEdit {
            file_ops: vec![FileOp::Create {
                file: a.clone(),
                contents: "hi".to_string(),
            }],
            text_edits: Vec::new(),
        };
        let edit2 = WorkspaceEdit {
            file_ops: vec![FileOp::Rename { from: a, to: b }],
            text_edits: Vec::new(),
        };

        let err = edit1.merge(edit2).unwrap_err();
        assert!(matches!(err, EditError::FileOpCollision { .. }));
    }

    #[test]
    fn merge_merges_inserts_at_same_position_deterministically() {
        let file = FileId::new("file:///test");

        let edit_a = WorkspaceEdit::new(vec![TextEdit::insert(file.clone(), 0, "a")]);
        let edit_b = WorkspaceEdit::new(vec![TextEdit::insert(file.clone(), 0, "b")]);

        let merged_ab = edit_a.clone().merge(edit_b.clone()).unwrap();
        let merged_ba = edit_b.merge(edit_a).unwrap();

        assert_eq!(merged_ab, merged_ba);
        assert_eq!(merged_ab.text_edits, vec![TextEdit::insert(file, 0, "ab")]);
    }

    #[test]
    fn apply_text_edits_errors_on_invalid_utf8_boundaries() {
        let file = FileId::new("file:///test");
        let original = "aé"; // 'é' is two bytes in UTF-8; valid boundaries are 0, 1, 3.

        let edit = TextEdit::replace(file.clone(), TextRange::new(2, 3), "e");
        let err = apply_text_edits(original, &[edit]).unwrap_err();

        assert_eq!(err, EditError::InvalidUtf8Boundary { file, offset: 2 },);
    }

    #[test]
    fn apply_text_edits_accepts_valid_utf8_boundaries() {
        let file = FileId::new("file:///test");
        let original = "aé";

        // Replace the whole 'é' character (byte range 1..3).
        let edit = TextEdit::replace(file, TextRange::new(1, 3), "e");
        let updated = apply_text_edits(original, &[edit]).unwrap();

        assert_eq!(updated, "ae");
    }

    #[test]
    fn apply_text_edits_rejects_ranges_that_split_utf8_characters() {
        // 😀 is 4 bytes in UTF-8: "a😀b" has byte indices:
        // a: 0..1, 😀: 1..5, b: 5..6.
        // Use a range inside the emoji byte sequence to ensure we never panic in `replace_range`.
        let file = FileId::new("file:///test");
        let original = "a😀b";

        let edit = TextEdit::replace(file.clone(), TextRange::new(2, 3), "X");
        let err = apply_text_edits(original, &[edit]).unwrap_err();

        assert_eq!(err, EditError::InvalidUtf8Boundary { file, offset: 2 },);
    }
}
