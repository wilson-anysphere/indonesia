use crate::CompletionContextId;
use lsp_types::{CompletionItem, InsertTextFormat};
use nova_ai::{AdditionalEdit, MultiTokenInsertTextFormat};
use nova_ide::{CompletionSource, NovaCompletionItem};
use serde_json::Value;

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

    let confidence = item
        .confidence
        .map(|value| value as f64)
        .filter(|value| value.is_finite());

    let data = Value::Object({
        let mut nova = serde_json::Map::new();
        nova.insert(
            "completion_context_id".to_string(),
            Value::String(context_id.to_string()),
        );
        nova.insert(
            "source".to_string(),
            Value::String(
                match item.source {
                    CompletionSource::Standard => "standard",
                    CompletionSource::Ai => "ai",
                }
                .to_string(),
            ),
        );
        nova.insert(
            "confidence".to_string(),
            confidence.map(Value::from).unwrap_or(Value::Null),
        );
        if !imports.is_empty() {
            nova.insert(
                "imports".to_string(),
                Value::Array(imports.into_iter().map(Value::String).collect()),
            );
        }

        let mut data = serde_json::Map::new();
        data.insert("nova".to_string(), Value::Object(nova));
        data
    });

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
