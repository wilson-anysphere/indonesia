use nova_fuzzy::{FuzzyMatcher, MatchScore};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionItem {
    pub label: String,
}

#[derive(Debug, Clone)]
struct RankedCompletion {
    item: CompletionItem,
    score: MatchScore,
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
            CompletionItem {
                label: "foobar".into(),
            },
            CompletionItem {
                label: "barfoo".into(),
            },
        ];
        let ranked = filter_and_rank_completions(items, "foo", 10);
        assert_eq!(ranked[0].label, "foobar");
    }
}
