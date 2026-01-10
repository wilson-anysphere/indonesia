use nova_ai::{AdditionalEdit, MultiTokenInsertTextFormat};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionSource {
    Standard,
    Ai,
}

/// Nova's internal completion model.
#[derive(Clone, Debug, PartialEq)]
pub struct NovaCompletionItem {
    pub label: String,
    pub insert_text: String,
    pub format: MultiTokenInsertTextFormat,
    pub additional_edits: Vec<AdditionalEdit>,
    pub detail: Option<String>,
    pub source: CompletionSource,
    /// Confidence score provided for AI completions.
    pub confidence: Option<f32>,
}

impl NovaCompletionItem {
    pub fn standard(label: impl Into<String>, insert_text: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            insert_text: insert_text.into(),
            format: MultiTokenInsertTextFormat::PlainText,
            additional_edits: Vec::new(),
            detail: None,
            source: CompletionSource::Standard,
            confidence: None,
        }
    }

    pub fn ai(
        label: impl Into<String>,
        insert_text: impl Into<String>,
        format: MultiTokenInsertTextFormat,
        additional_edits: Vec<AdditionalEdit>,
        confidence: f32,
    ) -> Self {
        Self {
            label: label.into(),
            insert_text: insert_text.into(),
            format,
            additional_edits,
            detail: Some(format!("AI • confidence {:.2}", confidence.clamp(0.0, 1.0))),
            source: CompletionSource::Ai,
            confidence: Some(confidence),
        }
    }
}
