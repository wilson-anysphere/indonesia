//! Text edit primitives and utilities.

use crate::{FileId, TextRange, TextSize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextEdit {
    pub range: TextRange,
    pub replacement: String,
}

impl TextEdit {
    pub fn new(range: TextRange, replacement: impl Into<String>) -> Self {
        Self {
            range,
            replacement: replacement.into(),
        }
    }

    pub fn insert(offset: TextSize, text: impl Into<String>) -> Self {
        Self::new(TextRange::new(offset, offset), text)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WorkspaceEdit {
    pub changes: BTreeMap<FileId, Vec<TextEdit>>,
}

impl WorkspaceEdit {
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    pub fn add_edit(&mut self, file: FileId, edit: TextEdit) {
        self.changes.entry(file).or_default().push(edit);
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum EditError {
    RangeOutOfBounds {
        range: TextRange,
        text_len: TextSize,
    },
    InvalidUtf8Boundary {
        offset: TextSize,
    },
    OverlappingEdits {
        first: TextRange,
        second: TextRange,
    },
}

impl std::fmt::Display for EditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EditError::RangeOutOfBounds { range, text_len } => write!(
                f,
                "edit range {range:?} is out of bounds for text length {text_len:?}"
            ),
            EditError::InvalidUtf8Boundary { offset } => {
                write!(f, "offset {offset:?} is not a UTF-8 character boundary")
            }
            EditError::OverlappingEdits { first, second } => {
                write!(f, "overlapping edits: {first:?} overlaps {second:?}")
            }
        }
    }
}

impl std::error::Error for EditError {}

/// Apply a list of edits to a text snapshot.
///
/// The function is deterministic: edits are first sorted by `(start, end)` and
/// applied from the end of the text backwards.
pub fn apply_text_edits(text: &str, edits: &[TextEdit]) -> Result<String, EditError> {
    let mut edits = edits.to_vec();
    normalize_text_edits(text, &mut edits)?;

    let mut out = text.to_string();
    for edit in edits.into_iter().rev() {
        let start = u32::from(edit.range.start()) as usize;
        let end = u32::from(edit.range.end()) as usize;
        debug_assert!(out.is_char_boundary(start) && out.is_char_boundary(end));
        out.replace_range(start..end, &edit.replacement);
    }
    Ok(out)
}

/// Sort edits and check for overlaps / out-of-bounds.
pub fn normalize_text_edits(text: &str, edits: &mut Vec<TextEdit>) -> Result<(), EditError> {
    edits.sort_by_key(|e| (e.range.start(), e.range.end()));

    let text_len = TextSize::from(text.len() as u32);

    for edit in edits.iter() {
        if edit.range.start() > edit.range.end() || edit.range.end() > text_len {
            return Err(EditError::RangeOutOfBounds {
                range: edit.range,
                text_len,
            });
        }

        let start = u32::from(edit.range.start()) as usize;
        let end = u32::from(edit.range.end()) as usize;
        if !text.is_char_boundary(start) {
            return Err(EditError::InvalidUtf8Boundary {
                offset: edit.range.start(),
            });
        }
        if !text.is_char_boundary(end) {
            return Err(EditError::InvalidUtf8Boundary {
                offset: edit.range.end(),
            });
        }
    }

    for pair in edits.windows(2) {
        let first = &pair[0];
        let second = &pair[1];
        if first.range.end() > second.range.start()
            || (first.range.start() == first.range.end()
                && second.range.start() == second.range.end()
                && first.range.start() == second.range.start())
        {
            return Err(EditError::OverlappingEdits {
                first: first.range,
                second: second.range,
            });
        }
    }

    // Coalesce adjacent edits (e.g. two back-to-back inserts/replacements).
    let mut merged: Vec<TextEdit> = Vec::with_capacity(edits.len());
    for edit in edits.drain(..) {
        if let Some(last) = merged.last_mut() {
            if last.range.end() == edit.range.start() {
                last.range = TextRange::new(last.range.start(), edit.range.end());
                last.replacement.push_str(&edit.replacement);
                continue;
            }
        }
        merged.push(edit);
    }
    *edits = merged;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_multiple_edits_is_deterministic() {
        let text = "abcdef";
        let mut edits = vec![
            // Replace "cd" -> "XX"
            TextEdit::new(TextRange::new(TextSize::from(2), TextSize::from(4)), "XX"),
            // Insert "!" at start
            TextEdit::insert(TextSize::from(0), "!"),
            // Delete "f"
            TextEdit::new(TextRange::new(TextSize::from(5), TextSize::from(6)), ""),
        ];

        let out1 = apply_text_edits(text, &edits).unwrap();

        edits.reverse();
        let out2 = apply_text_edits(text, &edits).unwrap();

        assert_eq!(out1, out2);
        assert_eq!(out1, "!abXXe");
    }

    #[test]
    fn detect_overlapping_edits() {
        let text = "abcdef";
        let edits = vec![
            TextEdit::new(TextRange::new(TextSize::from(1), TextSize::from(4)), "X"),
            TextEdit::new(TextRange::new(TextSize::from(3), TextSize::from(5)), "Y"),
        ];

        assert!(matches!(
            apply_text_edits(text, &edits),
            Err(EditError::OverlappingEdits { .. })
        ));
    }
}
