use std::fmt;
use std::sync::Arc;

use nova_core::{Position, Range, TextEdit, TextRange, TextSize};

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

#[cfg(feature = "lsp")]
impl From<lsp_types::TextDocumentContentChangeEvent> for ContentChange {
    fn from(value: lsp_types::TextDocumentContentChangeEvent) -> Self {
        Self {
            range: value.range.map(Into::into),
            text: value.text,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocumentError {
    DocumentNotOpen,
    InvalidRange,
}

impl fmt::Display for DocumentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DocumentError::DocumentNotOpen => write!(f, "document not open"),
            DocumentError::InvalidRange => write!(f, "invalid range"),
        }
    }
}

impl std::error::Error for DocumentError {}

/// An in-memory document with versioning and incremental edits.
#[derive(Debug, Clone)]
pub struct Document {
    text: Arc<String>,
    version: i32,
    line_offsets: Vec<usize>,
}

impl Document {
    pub fn new(text: Arc<String>, version: i32) -> Self {
        let line_offsets = compute_line_offsets(&text);
        Self {
            text,
            version,
            line_offsets,
        }
    }

    pub fn new_string(text: impl Into<String>, version: i32) -> Self {
        Self::new(Arc::new(text.into()), version)
    }

    pub fn text(&self) -> &str {
        self.text.as_str()
    }

    pub fn text_arc(&self) -> Arc<String> {
        Arc::clone(&self.text)
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
                (Range::new(Position::new(0, 0), end), change.text.clone())
            }
        };

        let start = self.position_to_offset(range.start);
        let end = self.position_to_offset(range.end);
        if start > end || end > self.text.len() {
            return Err(DocumentError::InvalidRange);
        }

        let text = Arc::make_mut(&mut self.text);
        text.replace_range(start..end, &replacement);
        self.line_offsets = compute_line_offsets(text);

        let start = u32::try_from(start).map_err(|_| DocumentError::InvalidRange)?;
        let end = u32::try_from(end).map_err(|_| DocumentError::InvalidRange)?;
        Ok(TextEdit::new(
            TextRange::new(TextSize::from(start), TextSize::from(end)),
            replacement,
        ))
    }

    fn end_position(&self) -> Position {
        let last_line = self.line_offsets.len().saturating_sub(1) as u32;
        let line_start = *self.line_offsets.last().unwrap_or(&0);
        let line_text = &self.text[line_start..];
        Position::new(last_line, utf16_len(line_text) as u32)
    }

    fn position_to_offset(&self, position: Position) -> usize {
        let line = position.line as usize;
        if line >= self.line_offsets.len() {
            return self.text.len();
        }

        let line_start = self.line_offsets[line];

        let mut line_end = if line + 1 < self.line_offsets.len() {
            self.line_offsets[line + 1]
        } else {
            self.text.len()
        };

        // Exclude the line terminator from column calculations. LSP positions are
        // defined over the line text, not including `\n` (and also `\r\n`).
        if line_end > line_start {
            let bytes = self.text.as_bytes();
            if bytes[line_end - 1] == b'\n' {
                line_end -= 1;
                if line_end > line_start && bytes[line_end - 1] == b'\r' {
                    line_end -= 1;
                }
            } else if bytes[line_end - 1] == b'\r' {
                line_end -= 1;
            }
        }

        let line_slice = &self.text[line_start..line_end];
        let rel = utf16_column_to_byte_offset_clamped(line_slice, position.character);
        line_start + rel
    }
}

fn compute_line_offsets(text: &str) -> Vec<usize> {
    let mut offsets = vec![0];
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                offsets.push(i + 1);
                i += 1;
            }
            b'\r' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                    offsets.push(i + 2);
                    i += 2;
                } else {
                    offsets.push(i + 1);
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    offsets
}

fn utf16_len(s: &str) -> usize {
    s.chars().map(|c| c.len_utf16()).sum()
}

/// Converts a UTF-16 code unit column into a byte offset into `line`.
///
/// The conversion is *clamped*:
/// - columns past the end of the line map to the line end
/// - columns that split a multi-code-unit character map to the start of that character
fn utf16_column_to_byte_offset_clamped(line: &str, column_utf16: u32) -> usize {
    let mut col: u32 = 0;
    for (idx, ch) in line.char_indices() {
        let ch_len = ch.len_utf16() as u32;
        if col >= column_utf16 {
            return idx;
        }
        if col + ch_len > column_utf16 {
            return idx;
        }
        col = col.saturating_add(ch_len);
    }
    line.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_incremental_edit() {
        let mut doc = Document::new_string("hello world\n", 1);
        let range = Range::new(Position::new(0, 6), Position::new(0, 11));
        let edits = doc
            .apply_changes(2, &[ContentChange::replace(range, "nova")])
            .unwrap();

        assert_eq!(doc.text(), "hello nova\n");
        assert_eq!(doc.version(), 2);
        assert_eq!(edits.len(), 1);
        assert_eq!(
            edits[0].range,
            TextRange::new(TextSize::from(6), TextSize::from(11))
        );
        assert_eq!(edits[0].replacement, "nova");
    }

    #[test]
    fn applies_full_replacement_and_normalizes_range() {
        let mut doc = Document::new_string("a\nb\n", 1);
        let edits = doc.apply_changes(2, &[ContentChange::full("x")]).unwrap();

        assert_eq!(doc.text(), "x");
        assert_eq!(doc.version(), 2);
        assert_eq!(edits.len(), 1);
        assert_eq!(
            edits[0].range,
            TextRange::new(TextSize::from(0), TextSize::from(4))
        );
    }

    #[test]
    fn utf16_positions_are_supported() {
        // U+10400 (DESERET CAPITAL LETTER LONG I) is a surrogate pair in UTF-16.
        let mut doc = Document::new_string("a𐐀b", 1);
        let range = Range::new(Position::new(0, 1), Position::new(0, 3));
        doc.apply_changes(2, &[ContentChange::replace(range, "X")])
            .unwrap();

        assert_eq!(doc.text(), "aXb");
    }

    #[test]
    fn clamps_out_of_bounds_character_offsets() {
        let mut doc = Document::new_string("a\r\nb", 1);
        // Line 0 is just "a" (CRLF is the line terminator and not part of the line).
        let range = Range::new(Position::new(0, 2), Position::new(0, 2));
        doc.apply_changes(2, &[ContentChange::replace(range, "X")])
            .unwrap();
        assert_eq!(doc.text(), "aX\r\nb");
    }

    #[test]
    fn handles_cr_only_newlines() {
        let mut doc = Document::new_string("a\rb", 1);
        // Line 0 is just "a" (CR is treated as the line terminator).
        let range = Range::new(Position::new(0, 2), Position::new(0, 2));
        doc.apply_changes(2, &[ContentChange::replace(range, "X")])
            .unwrap();
        assert_eq!(doc.text(), "aX\rb");
    }

    #[test]
    fn clamps_positions_inside_surrogate_pairs() {
        let mut doc = Document::new_string("a𐐀b", 1);
        // UTF-16 column 2 falls between the surrogate pair code units.
        let range = Range::new(Position::new(0, 2), Position::new(0, 2));
        doc.apply_changes(2, &[ContentChange::replace(range, "X")])
            .unwrap();
        assert_eq!(doc.text(), "aX𐐀b");
    }
}
