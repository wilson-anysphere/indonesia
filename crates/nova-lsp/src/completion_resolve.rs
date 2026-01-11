use std::collections::{BTreeMap, HashSet};

use lsp_types::{CompletionItem, TextEdit};

use crate::imports::java_import_text_edit;

/// Best-effort implementation of `completionItem/resolve` for Nova-provided
/// completion items.
///
/// When the completion item has requested import insertions stashed in
/// `CompletionItem.data.nova.imports`, this computes correct `additionalTextEdits`
/// based on the current document text.
#[must_use]
pub fn resolve_completion_item(mut item: CompletionItem, document_text: &str) -> CompletionItem {
    let imports = import_paths_from_item_data(&item);
    if imports.is_empty() {
        return item;
    }

    let mut new_edits = java_import_text_edits(document_text, &imports);
    if new_edits.is_empty() {
        return item;
    }

    match item.additional_text_edits.as_mut() {
        Some(existing) => existing.append(&mut new_edits),
        None => item.additional_text_edits = Some(new_edits),
    }

    item
}

fn import_paths_from_item_data(item: &CompletionItem) -> Vec<String> {
    let Some(data) = item.data.as_ref() else {
        return Vec::new();
    };
    let Some(imports) = data
        .get("nova")
        .and_then(|nova| nova.get("imports"))
        .and_then(|imports| imports.as_array())
    else {
        return Vec::new();
    };

    let mut seen: HashSet<&str> = HashSet::new();
    let mut out = Vec::new();
    for value in imports {
        let Some(path) = value.as_str() else {
            continue;
        };
        if seen.insert(path) {
            out.push(path.to_string());
        }
    }
    out
}

fn java_import_text_edits(document_text: &str, imports: &[String]) -> Vec<TextEdit> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut grouped: BTreeMap<(u32, u32, u32, u32), (lsp_types::Range, String)> = BTreeMap::new();

    for path in imports {
        if !seen.insert(path.clone()) {
            continue;
        }

        let Some(edit) = java_import_text_edit(document_text, path) else {
            continue;
        };

        let key = (
            edit.range.start.line,
            edit.range.start.character,
            edit.range.end.line,
            edit.range.end.character,
        );
        let entry = grouped
            .entry(key)
            .or_insert_with(|| (edit.range.clone(), String::new()));
        entry.1.push_str(&edit.new_text);
    }

    grouped
        .into_values()
        .map(|(range, new_text)| TextEdit { range, new_text })
        .collect()
}

#[cfg(all(test, feature = "ai"))]
mod tests {
    use super::*;
    use crate::to_lsp::to_lsp_completion_item;
    use crate::CompletionContextId;
    use lsp_types::{Position, Range};
    use nova_ai::{AdditionalEdit, MultiTokenInsertTextFormat};
    use nova_ide::NovaCompletionItem;
    use pretty_assertions::assert_eq;

    #[test]
    fn completion_item_resolve_adds_import_edit() {
        let context_id: CompletionContextId = "1".parse().expect("context id");
        let item = NovaCompletionItem::ai(
            "collect".to_string(),
            "collect(Collectors.toList())".to_string(),
            MultiTokenInsertTextFormat::PlainText,
            vec![AdditionalEdit::AddImport {
                path: "java.util.stream.Collectors".to_string(),
            }],
            0.9,
        );

        let lsp_item = to_lsp_completion_item(item, &context_id);
        assert!(
            lsp_item.additional_text_edits.is_none(),
            "completion items should not eagerly compute import edits"
        );

        let data = lsp_item.data.clone().expect("data present");
        assert_eq!(
            data.get("nova")
                .and_then(|nova| nova.get("imports"))
                .and_then(|imports| imports.as_array())
                .and_then(|imports| imports.first())
                .and_then(|value| value.as_str()),
            Some("java.util.stream.Collectors"),
        );

        let document_text = "package com.example;\n\nclass Foo {}\n";
        let resolved = resolve_completion_item(lsp_item, document_text);
        let edits = resolved.additional_text_edits.expect("additional edits");
        assert_eq!(edits.len(), 1);
        assert_eq!(
            edits[0].range,
            Range::new(Position::new(1, 0), Position::new(1, 0))
        );
        assert_eq!(edits[0].new_text, "import java.util.stream.Collectors;\n");
    }
}
