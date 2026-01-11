use crate::CompletionContextId;
use lsp_types::{CompletionItem, InsertTextFormat};
use nova_ai::{AdditionalEdit, MultiTokenInsertTextFormat};
use nova_ide::{CompletionSource, NovaCompletionItem};
use serde_json::json;

pub fn to_lsp_completion_item(
    item: NovaCompletionItem,
    context_id: &CompletionContextId,
) -> CompletionItem {
    let mut imports = Vec::new();
    for edit in &item.additional_edits {
        let AdditionalEdit::AddImport { path } = edit;
        if !imports.contains(path) {
            imports.push(path.clone());
        }
    }

    let mut data = json!({
        "nova": {
            "completion_context_id": context_id.to_string(),
            "source": match item.source {
                CompletionSource::Standard => "standard",
                CompletionSource::Ai => "ai",
            },
            "confidence": item.confidence,
        }
    });
    if !imports.is_empty() {
        data["nova"]["imports"] = json!(imports);
    }

    CompletionItem {
        label: item.label,
        insert_text: Some(item.insert_text),
        insert_text_format: Some(match item.format {
            MultiTokenInsertTextFormat::PlainText => InsertTextFormat::PLAIN_TEXT,
            MultiTokenInsertTextFormat::Snippet => InsertTextFormat::SNIPPET,
        }),
        detail: item.detail,
        data: Some(data),
        ..CompletionItem::default()
    }
}
