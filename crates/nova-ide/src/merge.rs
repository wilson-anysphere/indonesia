use crate::{CompletionConfig, CompletionSource, NovaCompletionItem};
use std::collections::HashSet;

/// Merge AI completion items with standard completions.
///
/// - AI items are placed at the top
/// - AI items are sorted by confidence (descending) then label for determinism
/// - Duplicates by `insert_text` are removed (AI loses to Standard)
pub fn merge_completions(
    mut standard: Vec<NovaCompletionItem>,
    mut ai: Vec<NovaCompletionItem>,
    config: &CompletionConfig,
) -> Vec<NovaCompletionItem> {
    if !config.ai_enabled {
        return standard;
    }

    let mut seen_insert_text: HashSet<String> =
        standard.iter().map(|item| item.insert_text.clone()).collect();

    ai.retain(|item| seen_insert_text.insert(item.insert_text.clone()));

    // Also deduplicate within AI items. If we see duplicates, keep the earliest (higher confidence due
    // to sorting below).
    let mut seen_ai = HashSet::new();
    ai.retain(|item| seen_ai.insert(item.insert_text.clone()));

    ai.sort_by(|a, b| {
        let a_conf = a.confidence.unwrap_or(0.0);
        let b_conf = b.confidence.unwrap_or(0.0);
        b_conf
            .total_cmp(&a_conf)
            .then_with(|| a.label.cmp(&b.label))
    });

    if ai.len() > config.ai_max_items {
        ai.truncate(config.ai_max_items);
    }

    // Make sure standard completions remain in original order.
    standard.retain(|item| item.source == CompletionSource::Standard);

    ai.extend(standard);
    ai
}

#[cfg(test)]
mod tests {
    use super::*;
    use nova_ai::MultiTokenInsertTextFormat;

    #[test]
    fn merge_is_deterministic_and_ai_first() {
        let config = CompletionConfig {
            ai_enabled: true,
            ai_max_items: 10,
            ai_max_additional_edits: 3,
            ai_max_tokens: 64,
        };

        let standard = vec![
            NovaCompletionItem::standard("collect", "collect"),
            NovaCompletionItem::standard("count", "count"),
        ];

        let ai = vec![
            NovaCompletionItem {
                label: "b".into(),
                insert_text: "filter(...)".into(),
                format: MultiTokenInsertTextFormat::Snippet,
                additional_edits: vec![],
                detail: Some("AI".into()),
                source: CompletionSource::Ai,
                confidence: Some(0.9),
            },
            NovaCompletionItem {
                label: "a".into(),
                insert_text: "map(...)".into(),
                format: MultiTokenInsertTextFormat::Snippet,
                additional_edits: vec![],
                detail: Some("AI".into()),
                source: CompletionSource::Ai,
                confidence: Some(0.9),
            },
            // Duplicate insert text; should be removed.
            NovaCompletionItem {
                label: "dup".into(),
                insert_text: "collect".into(),
                format: MultiTokenInsertTextFormat::PlainText,
                additional_edits: vec![],
                detail: Some("AI".into()),
                source: CompletionSource::Ai,
                confidence: Some(1.0),
            },
        ];

        let merged = merge_completions(standard, ai, &config);
        let labels: Vec<_> = merged.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, vec!["a", "b", "collect", "count"]);
    }
}
