/// How the completion text should be interpreted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MultiTokenInsertTextFormat {
    PlainText,
    Snippet,
}

/// Additional edits requested by the AI completion.
///
/// For now this is intentionally limited to import insertions to keep the
/// validation surface small and predictable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdditionalEdit {
    AddImport { path: String },
}

/// A structured AI completion suggestion.
#[derive(Clone, Debug, PartialEq)]
pub struct MultiTokenCompletion {
    /// Label shown in the UI.
    pub label: String,
    /// Insert text. May contain snippet placeholders when `format` is `Snippet`.
    pub insert_text: String,
    pub format: MultiTokenInsertTextFormat,
    /// Additional edits, typically used for imports.
    pub additional_edits: Vec<AdditionalEdit>,
    /// A provider-supplied confidence score in the range `[0, 1]`.
    pub confidence: f32,
}
