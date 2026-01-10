use crate::CompletionContextId;
use lsp_types::{CompletionItem, InsertTextFormat, Position, Range, TextEdit};
use nova_ai::{AdditionalEdit, MultiTokenInsertTextFormat};
use nova_ide::{CompletionSource, NovaCompletionItem};
use serde_json::json;

pub fn to_lsp_completion_item(
    item: NovaCompletionItem,
    context_id: &CompletionContextId,
) -> CompletionItem {
    let mut additional_text_edits = Vec::new();
    for edit in &item.additional_edits {
        match edit {
            AdditionalEdit::AddImport { path } => {
                additional_text_edits.push(TextEdit {
                    range: Range::new(Position::new(0, 0), Position::new(0, 0)),
                    new_text: format!("import {};\n", path),
                });
            }
        }
    }

    CompletionItem {
        label: item.label,
        insert_text: Some(item.insert_text),
        insert_text_format: Some(match item.format {
            MultiTokenInsertTextFormat::PlainText => InsertTextFormat::PLAIN_TEXT,
            MultiTokenInsertTextFormat::Snippet => InsertTextFormat::SNIPPET,
        }),
        additional_text_edits: (!additional_text_edits.is_empty()).then_some(additional_text_edits),
        detail: item.detail,
        data: Some(json!({
            "nova": {
                "completion_context_id": context_id.to_string(),
                "source": match item.source {
                    CompletionSource::Standard => "standard",
                    CompletionSource::Ai => "ai",
                },
                "confidence": item.confidence,
            }
        })),
        ..CompletionItem::default()
    }
}
