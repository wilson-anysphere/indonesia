use nova_core::{CompletionItem, CompletionItemKind};
use nova_fuzzy::{FuzzyMatcher, MatchScore};

#[derive(Debug, Clone)]
struct RankedCompletion {
    item: CompletionItem,
    score: MatchScore,
}

fn kind_weight(kind: CompletionItemKind) -> i32 {
    match kind {
        CompletionItemKind::Method => 100,
        CompletionItemKind::Field => 80,
        CompletionItemKind::Variable => 70,
        CompletionItemKind::Class => 60,
        CompletionItemKind::Snippet => 50,
        CompletionItemKind::Keyword => 10,
        CompletionItemKind::Other => 0,
    }
}

/// Filter and rank completion items for `query`.
///
/// This is intended for relatively small completion lists (typically < 1000),
/// so we score all items rather than building an index.
pub fn filter_and_rank_completions(
    items: impl IntoIterator<Item = CompletionItem>,
    query: &str,
    limit: usize,
) -> Vec<CompletionItem> {
    let mut matcher = FuzzyMatcher::new(query);
    let mut ranked: Vec<RankedCompletion> = items
        .into_iter()
        .filter_map(|item| {
            let score = matcher.score(&item.label)?;
            Some(RankedCompletion { item, score })
        })
        .collect();

    ranked.sort_by(|a, b| {
        b.score
            .rank_key()
            .cmp(&a.score.rank_key())
            .then_with(|| kind_weight(b.item.kind).cmp(&kind_weight(a.item.kind)))
            .then_with(|| a.item.label.len().cmp(&b.item.label.len()))
            .then_with(|| a.item.label.cmp(&b.item.label))
    });

    ranked.truncate(limit);
    ranked.into_iter().map(|r| r.item).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_prefix_ranks_first() {
        let items = vec![
            CompletionItem::new("foobar", CompletionItemKind::Other),
            CompletionItem::new("barfoo", CompletionItemKind::Other),
        ];
        let ranked = filter_and_rank_completions(items, "foo", 10);
        assert_eq!(ranked[0].label, "foobar");
    }
}
