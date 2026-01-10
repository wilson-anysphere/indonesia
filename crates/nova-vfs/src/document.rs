use std::fmt;

use nova_core::{Position, Range, TextEdit};

/// An LSP-style content change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentChange {
    /// The range of text to replace. If `None`, the entire document is replaced.
    pub range: Option<Range>,
    /// Replacement text.
    pub text: String,
}

impl ContentChange {
    pub fn full(text: impl Into<String>) -> Self {
        Self {
            range: None,
            text: text.into(),
        }
    }

    pub fn replace(range: Range, text: impl Into<String>) -> Self {
        Self {
            range: Some(range),
            text: text.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocumentError {
    InvalidRange,
    InvalidPosition,
}

impl fmt::Display for DocumentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DocumentError::InvalidRange => write!(f, "invalid range"),
            DocumentError::InvalidPosition => write!(f, "invalid position"),
        }
    }
}

impl std::error::Error for DocumentError {}

/// An in-memory document with versioning and incremental edits.
#[derive(Debug, Clone)]
pub struct Document {
    text: String,
    version: i32,
    line_offsets: Vec<usize>,
}

impl Document {
    pub fn new(text: impl Into<String>, version: i32) -> Self {
        let text = text.into();
        let line_offsets = compute_line_offsets(&text);
        Self {
            text,
            version,
            line_offsets,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn version(&self) -> i32 {
        self.version
    }

    /// Applies a sequence of incremental LSP changes in order and returns the normalized edits.
    pub fn apply_changes(
        &mut self,
        new_version: i32,
        changes: &[ContentChange],
    ) -> Result<Vec<TextEdit>, DocumentError> {
        let mut edits = Vec::with_capacity(changes.len());

        for change in changes {
            let edit = self.apply_change(change)?;
            edits.push(edit);
        }

        self.version = new_version;
        Ok(edits)
    }

    fn apply_change(&mut self, change: &ContentChange) -> Result<TextEdit, DocumentError> {
        let (range, replacement) = match &change.range {
            Some(range) => (*range, change.text.clone()),
            None => {
                let end = self.end_position();
                (
                    Range::new(Position::new(0, 0), end),
                    change.text.clone(),
                )
            }
        };

        let start = self.position_to_offset(range.start)?;
        let end = self.position_to_offset(range.end)?;
        if start > end || end > self.text.len() {
            return Err(DocumentError::InvalidRange);
        }

        self.text.replace_range(start..end, &replacement);
        self.line_offsets = compute_line_offsets(&self.text);

        Ok(TextEdit::new(range, replacement))
    }

    fn end_position(&self) -> Position {
        let last_line = self.line_offsets.len().saturating_sub(1) as u32;
        let line_start = *self.line_offsets.last().unwrap_or(&0);
        let line_text = &self.text[line_start..];
        Position::new(last_line, utf16_len(line_text) as u32)
    }

    fn position_to_offset(&self, position: Position) -> Result<usize, DocumentError> {
        let line = position.line as usize;
        let line_start = *self
            .line_offsets
            .get(line)
            .ok_or(DocumentError::InvalidPosition)?;

        let line_end = if line + 1 < self.line_offsets.len() {
            self.line_offsets[line + 1]
        } else {
            self.text.len()
        };
        let line_slice = &self.text[line_start..line_end];

        let rel = utf16_column_to_byte_offset(line_slice, position.character)
            .ok_or(DocumentError::InvalidPosition)?;
        Ok(line_start + rel)
    }
}

fn compute_line_offsets(text: &str) -> Vec<usize> {
    let mut offsets = vec![0];
    for (idx, byte) in text.bytes().enumerate() {
        if byte == b'\n' {
            offsets.push(idx + 1);
        }
    }
    offsets
}

fn utf16_len(s: &str) -> usize {
    s.chars().map(|c| c.len_utf16()).sum()
}

/// Converts a UTF-16 code unit column into a byte offset into `line`.
///
/// Returns `None` if the column is out of range or splits a surrogate pair.
fn utf16_column_to_byte_offset(line: &str, column_utf16: u32) -> Option<usize> {
    let mut col: u32 = 0;
    for (idx, ch) in line.char_indices() {
        let ch_len = ch.len_utf16() as u32;
        if col == column_utf16 {
            return Some(idx);
        }
        if col + ch_len > column_utf16 {
            return None;
        }
        col += ch_len;
    }
    if col == column_utf16 {
        Some(line.len())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_incremental_edit() {
        let mut doc = Document::new("hello world\n", 1);
        let range = Range::new(Position::new(0, 6), Position::new(0, 11));
        let edits = doc
            .apply_changes(2, &[ContentChange::replace(range, "nova")])
            .unwrap();

        assert_eq!(doc.text(), "hello nova\n");
        assert_eq!(doc.version(), 2);
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].range, range);
        assert_eq!(edits[0].new_text, "nova");
    }

    #[test]
    fn applies_full_replacement_and_normalizes_range() {
        let mut doc = Document::new("a\nb\n", 1);
        let edits = doc.apply_changes(2, &[ContentChange::full("x")]).unwrap();

        assert_eq!(doc.text(), "x");
        assert_eq!(doc.version(), 2);
        assert_eq!(edits.len(), 1);
        assert_eq!(
            edits[0].range,
            Range::new(Position::new(0, 0), Position::new(2, 0))
        );
    }

    #[test]
    fn utf16_positions_are_supported() {
        // U+10400 (DESERET CAPITAL LETTER LONG I) is a surrogate pair in UTF-16.
        let mut doc = Document::new("a𐐀b", 1);
        let range = Range::new(Position::new(0, 1), Position::new(0, 3));
        doc.apply_changes(2, &[ContentChange::replace(range, "X")])
            .unwrap();

        assert_eq!(doc.text(), "aXb");
    }
}

