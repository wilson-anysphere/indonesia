use crate::stdio_extensions_db::SingleFileDb;
use crate::stdio_paths::{load_document_text, path_from_uri};
use crate::stdio_text::position_to_offset_utf16;
use crate::ServerState;

use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionList, CompletionParams, CompletionTextEdit,
    Range as LspTypesRange, TextEdit,
};
use nova_db::FileId as DbFileId;
use nova_db::InMemoryFileStore;
use nova_ide::extensions::IdeExtensions;
#[cfg(feature = "ai")]
use nova_ide::multi_token_completion_context;
use serde_json::{Map, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

fn value_object_mut(value: &mut Value) -> &mut Map<String, Value> {
    if !matches!(value, Value::Object(_)) {
        *value = Value::Object(Map::new());
    }
    match value {
        Value::Object(map) => map,
        _ => unreachable!("value was just replaced with an object"),
    }
}

fn ensure_object_field<'a>(
    obj: &'a mut Map<String, Value>,
    key: &str,
) -> &'a mut Map<String, Value> {
    let entry = obj
        .entry(key.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    value_object_mut(entry)
}

#[cfg(feature = "ai")]
pub(super) fn handle_completion_more(
    params: serde_json::Value,
    state: &ServerState,
) -> Result<serde_json::Value, String> {
    let params: Map<String, Value> = crate::stdio_jsonrpc::decode_params(params)?;
    let context_id = params
        .get("context_id")
        .and_then(Value::as_str)
        .or_else(|| params.get("contextId").and_then(Value::as_str))
        .ok_or_else(|| "missing required `context_id`".to_string())?;

    let (items, is_incomplete) = state.completion_service.completion_more(context_id);
    let mut response = Map::new();
    response.insert(
        "items".to_string(),
        serde_json::to_value(items).map_err(|e| e.to_string())?,
    );
    response.insert("is_incomplete".to_string(), Value::Bool(is_incomplete));
    Ok(Value::Object(response))
}

pub(super) fn handle_completion(
    params: serde_json::Value,
    state: &ServerState,
    cancel: CancellationToken,
) -> Result<serde_json::Value, String> {
    let params: CompletionParams = crate::stdio_jsonrpc::decode_params(params)?;
    let uri = params.text_document_position.text_document.uri;
    let position = params.text_document_position.position;

    let Some(text) = load_document_text(state, uri.as_str()) else {
        return Err(format!("missing document text for `{}`", uri.as_str()));
    };

    let doc_path = path_from_uri(uri.as_str());
    let path = doc_path
        .clone()
        .unwrap_or_else(|| PathBuf::from(uri.as_str()));
    let mut db = InMemoryFileStore::new();
    let file: DbFileId = db.file_id_for_path(&path);
    db.set_file_text(file, text.clone());

    #[cfg(feature = "ai")]
    let (completion_context_id, has_more) = {
        let ai_excluded = doc_path
            .as_deref()
            .is_some_and(|path| crate::stdio_ai_privacy::is_ai_excluded_path(state, path));
        let has_more = state.completion_service.completion_engine().supports_ai() && !ai_excluded;
        let completion_context_id = if has_more {
            let document_uri = Some(uri.as_str().to_string());
            let ctx = multi_token_completion_context(&db, file, position);

            // `NovaCompletionService` is Tokio-driven; enter the runtime so
            // `tokio::spawn` inside the completion pipeline is available.
            let runtime = state.runtime.as_ref().ok_or_else(|| {
                "AI completions are enabled but the Tokio runtime is unavailable".to_string()
            })?;
            let _guard = runtime.enter();
            let response = state.completion_service.completion_with_document_uri(
                ctx,
                cancel.clone(),
                document_uri,
            );
            response.context_id.to_string()
        } else {
            // Even when AI completions are disabled, the client can still issue
            // `nova/completion/more` with this id; the handler will return an empty
            // result, mirroring the legacy stdio protocol behaviour.
            state.completion_service.allocate_context_id().to_string()
        };
        (Some(completion_context_id), has_more)
    };

    #[cfg(not(feature = "ai"))]
    let (completion_context_id, has_more) = (None::<String>, false);

    #[cfg(feature = "ai")]
    let mut items = if state.ai_config.enabled && state.ai_config.features.completion_ranking {
        if let Some(runtime) = state.runtime.as_ref() {
            runtime.block_on(nova_ide::completions_with_ai(
                &db,
                file,
                position,
                &state.ai_config,
            ))
        } else {
            nova_lsp::completion(&db, file, position)
        }
    } else {
        nova_lsp::completion(&db, file, position)
    };

    #[cfg(not(feature = "ai"))]
    let mut items = nova_lsp::completion(&db, file, position);

    // Merge extension-provided completions (WASM providers) after Nova's built-in list.
    let offset = position_to_offset_utf16(&text, position).unwrap_or(text.len());
    let ext_db = Arc::new(SingleFileDb::new(file, Some(path), text));
    let ide_extensions = IdeExtensions::with_registry(
        ext_db,
        Arc::clone(&state.config),
        nova_ext::ProjectId::new(0),
        state.extensions_registry.clone(),
    );
    let extension_items = ide_extensions
        .completions(cancel.clone(), file, offset)
        .into_iter()
        .map(|item| CompletionItem {
            label: item.label,
            detail: item.detail,
            ..CompletionItem::default()
        });
    items.extend(extension_items);

    if items.is_empty() && has_more {
        items.push(CompletionItem {
            label: "AI completions…".to_string(),
            kind: Some(CompletionItemKind::TEXT),
            sort_text: Some("\u{10FFFF}".to_string()),
            text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                range: LspTypesRange::new(position, position),
                new_text: String::new(),
            })),
            ..CompletionItem::default()
        });
    }
    for item in &mut items {
        let data = item.data.get_or_insert_with(|| Value::Object(Map::new()));
        let data = value_object_mut(data);
        let nova = ensure_object_field(data, "nova");

        nova.insert("uri".to_string(), Value::String(uri.as_str().to_string()));
        if let Some(id) = completion_context_id.as_deref() {
            nova.insert(
                "completion_context_id".to_string(),
                Value::String(id.to_string()),
            );
        }
    }
    let list = CompletionList {
        is_incomplete: has_more,
        items,
        ..CompletionList::default()
    };

    serde_json::to_value(list).map_err(|e| e.to_string())
}

pub(super) fn handle_completion_item_resolve(
    params: serde_json::Value,
    state: &ServerState,
) -> Result<serde_json::Value, String> {
    let item: CompletionItem = crate::stdio_jsonrpc::decode_params(params)?;
    let resolved = resolve_completion_item_with_state(item, state);
    serde_json::to_value(resolved).map_err(|e| e.to_string())
}

fn resolve_completion_item_with_state(item: CompletionItem, state: &ServerState) -> CompletionItem {
    let uri = completion_item_uri(&item);
    if let Some(uri) = uri {
        if let Some(text) = load_document_text(state, uri) {
            return nova_lsp::resolve_completion_item(item, &text);
        }
    }

    // Best-effort fallback: resolve against the only open document when the completion
    // item doesn't carry a URI.
    let open = state.analysis.vfs.open_documents().snapshot();
    if open.len() != 1 {
        return item;
    }
    let Some(file_id) = open.into_iter().next() else {
        return item;
    };
    let Some(text) = state.analysis.file_contents.get(&file_id) else {
        return item;
    };
    nova_lsp::resolve_completion_item(item, text.as_str())
}

fn completion_item_uri(item: &CompletionItem) -> Option<&str> {
    item.data
        .as_ref()
        .and_then(|data| data.get("nova"))
        .and_then(|nova| {
            nova.get("uri")
                .or_else(|| nova.get("document_uri"))
                .or_else(|| nova.get("documentUri"))
        })
        .and_then(|uri| uri.as_str())
}
