use std::collections::BTreeMap;

use thiserror::Error;

/// Identifier for a workspace file.
///
/// In a real Nova implementation this would likely be an interned ID or a URI.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileId(pub String);

impl FileId {
    pub fn new(path: impl Into<String>) -> Self {
        Self(path.into())
    }
}

/// A half-open text range `[start, end)` in UTF-8 byte offsets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TextRange {
    pub start: usize,
    pub end: usize,
}

impl TextRange {
    pub fn new(start: usize, end: usize) -> Self {
        assert!(start <= end, "invalid range: {start}..{end}");
        Self { start, end }
    }

    pub fn len(self) -> usize {
        self.end.saturating_sub(self.start)
    }

    pub fn is_empty(self) -> bool {
        self.start == self.end
    }

    pub fn contains(self, offset: usize) -> bool {
        self.start <= offset && offset < self.end
    }
}

/// A single file edit.
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

/// A set of edits across potentially multiple files.
///
/// The edits are expected to be normalized (sorted, deduplicated, non-overlapping)
/// before being applied or converted to LSP.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkspaceEdit {
    pub edits: Vec<TextEdit>,
}

impl WorkspaceEdit {
    pub fn new(edits: Vec<TextEdit>) -> Self {
        Self { edits }
    }

    /// Returns edits grouped by file in deterministic order.
    pub fn edits_by_file(&self) -> BTreeMap<&FileId, Vec<&TextEdit>> {
        let mut map: BTreeMap<&FileId, Vec<&TextEdit>> = BTreeMap::new();
        for edit in &self.edits {
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
    pub fn normalize(&mut self) -> Result<(), EditError> {
        self.edits.sort_by(|a, b| {
            a.file
                .cmp(&b.file)
                .then_with(|| a.range.start.cmp(&b.range.start))
                .then_with(|| a.range.end.cmp(&b.range.end))
                .then_with(|| a.replacement.cmp(&b.replacement))
        });

        // Exact duplicates are redundant.
        self.edits
            .dedup_by(|a, b| a.file == b.file && a.range == b.range && a.replacement == b.replacement);

        // Merge multiple inserts at the same position. This avoids ambiguous
        // ordering when applying edits and keeps the edit set deterministic.
        let mut merged: Vec<TextEdit> = Vec::with_capacity(self.edits.len());
        for edit in self.edits.drain(..) {
            if let Some(last) = merged.last_mut() {
                if last.file == edit.file && last.range == edit.range && last.range.is_empty() {
                    last.replacement.push_str(&edit.replacement);
                    continue;
                }

                if last.file == edit.file && last.range == edit.range && last.replacement != edit.replacement {
                    return Err(EditError::OverlappingEdits {
                        file: edit.file,
                        first: last.range,
                        second: edit.range,
                    });
                }
            }
            merged.push(edit);
        }

        self.edits = merged;

        // Validate non-overlap per file.
        let mut current_file: Option<&FileId> = None;
        let mut prev: Option<TextRange> = None;
        for edit in &self.edits {
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
}

#[derive(Debug, Error)]
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

        out.replace_range(edit.range.start..edit.range.end, &edit.replacement);
    }

    Ok(out)
}
