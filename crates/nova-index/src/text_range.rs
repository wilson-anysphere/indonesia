use serde::{Deserialize, Serialize};

/// A half-open byte range \([start, end)\) within a UTF-8 string.
///
/// Most callers in Nova operate on byte offsets for editor/model interop and to avoid
/// re-walking UTF-8 codepoints. When converting to LSP positions, use UTF-16 column
/// conversion utilities at the boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TextRange {
    pub start: usize,
    pub end: usize,
}

impl TextRange {
    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    pub fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn shift(self, delta: usize) -> Self {
        Self {
            start: self.start.saturating_add(delta),
            end: self.end.saturating_add(delta),
        }
    }
}
