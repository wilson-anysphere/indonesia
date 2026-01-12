use nova_core::{CompletionItem, CompletionItemKind};
use nova_fuzzy::{FuzzyMatcher, MatchScore};

#[derive(Debug, Clone)]
struct RankedCompletion {
    item: CompletionItem,
    score: MatchScore,
    orig_index: usize,
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

fn ranked_completion_cmp(a: &RankedCompletion, b: &RankedCompletion) -> std::cmp::Ordering {
    b.score
        .rank_key()
        .cmp(&a.score.rank_key())
        .then_with(|| kind_weight(b.item.kind).cmp(&kind_weight(a.item.kind)))
        .then_with(|| a.item.label.len().cmp(&b.item.label.len()))
        .then_with(|| a.item.label.cmp(&b.item.label))
        .then_with(|| a.orig_index.cmp(&b.orig_index))
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
    if limit == 0 {
        return Vec::new();
    }

    let mut matcher = FuzzyMatcher::new(query);
    let mut ranked: Vec<RankedCompletion> = items
        .into_iter()
        .enumerate()
        .filter_map(|(orig_index, item)| {
            let score = matcher.score(&item.label)?;
            Some(RankedCompletion {
                item,
                score,
                orig_index,
            })
        })
        .collect();

    if ranked.len() > limit {
        ranked.select_nth_unstable_by(limit, ranked_completion_cmp);
        ranked.truncate(limit);
    }

    ranked.sort_by(ranked_completion_cmp);

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

    #[test]
    fn completion_preserves_stable_order_for_equal_keys() {
        let items = vec![
            CompletionItem::new("dup", CompletionItemKind::Other),
            CompletionItem::new("dup", CompletionItemKind::Other),
            CompletionItem::new("dup", CompletionItemKind::Other),
        ];

        // Use `String` allocation addresses to distinguish equal labels. The stable sort semantics
        // should preserve input ordering for identical keys, and the top-k optimization must not
        // disturb that ordering when truncating.
        let expected = items
            .iter()
            .take(2)
            .map(|item| item.label.as_ptr())
            .collect::<Vec<_>>();

        let ranked = filter_and_rank_completions(items, "", 2);
        let got = ranked
            .iter()
            .map(|item| item.label.as_ptr())
            .collect::<Vec<_>>();

        assert_eq!(got, expected);
    }
}
