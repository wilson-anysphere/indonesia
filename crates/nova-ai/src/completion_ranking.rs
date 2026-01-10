use std::cmp::Ordering;
use std::time::Duration;

use futures::future::{BoxFuture, FutureExt};

use nova_core::{CompletionContext, CompletionItem, CompletionItemKind};

use crate::util;
use crate::AiConfig;

/// Async-friendly interface for completion ranking.
///
/// Cancellation is achieved by dropping the returned future; `nova-lsp`/`nova-ide`
/// should always wrap ranking calls in a short timeout to avoid blocking
/// interactive requests.
pub trait CompletionRanker: Send + Sync {
    fn rank_completions<'a>(
        &'a self,
        ctx: &'a CompletionContext,
        items: Vec<CompletionItem>,
    ) -> BoxFuture<'a, Vec<CompletionItem>>;
}

/// A baseline, non-ML completion ranker.
///
/// This exists so the rest of the system can integrate AI "hooks" without
/// requiring a model. The heuristics are intentionally simple but deterministic.
#[derive(Debug, Default, Copy, Clone)]
pub struct BaselineCompletionRanker;

impl BaselineCompletionRanker {
    fn score(ctx: &CompletionContext, item: &CompletionItem) -> i64 {
        let prefix = ctx.prefix.as_str();
        let label = item.label.as_str();

        let mut score: i64 = 0;

        if !prefix.is_empty() {
            if label.starts_with(prefix) {
                score += 10_000;
            } else if label.to_lowercase().starts_with(&prefix.to_lowercase()) {
                score += 8_000;
            } else if label.contains(prefix) {
                score += 4_000;
            } else if label.to_lowercase().contains(&prefix.to_lowercase()) {
                score += 2_000;
            }
        }

        // Prefer shorter labels when all else is equal (less typing).
        score -= label.len() as i64;

        // Prefer more "actionable" items.
        score += match item.kind {
            CompletionItemKind::Method => 100,
            CompletionItemKind::Field => 80,
            CompletionItemKind::Variable => 70,
            CompletionItemKind::Class => 60,
            CompletionItemKind::Keyword => 10,
            CompletionItemKind::Snippet => 50,
            CompletionItemKind::Other => 0,
        };

        score
    }
}

impl CompletionRanker for BaselineCompletionRanker {
    fn rank_completions<'a>(
        &'a self,
        ctx: &'a CompletionContext,
        mut items: Vec<CompletionItem>,
    ) -> BoxFuture<'a, Vec<CompletionItem>> {
        // Precompute per-item scores for deterministic sorting.
        let mut scored: Vec<(CompletionItem, i64)> = items
            .drain(..)
            .map(|item| {
                let score = Self::score(ctx, &item);
                (item, score)
            })
            .collect();

        scored.sort_by(
            |(a_item, a_score), (b_item, b_score)| match b_score.cmp(a_score) {
                Ordering::Equal => a_item.label.cmp(&b_item.label),
                other => other,
            },
        );

        let ranked: Vec<CompletionItem> = scored.into_iter().map(|(item, _)| item).collect();
        futures::future::ready(ranked).boxed()
    }
}

/// Run completion ranking with a timeout.
///
/// If ranking exceeds `timeout` (or panics), this returns `items` unchanged.
pub async fn rank_completions_with_timeout<R: CompletionRanker>(
    ranker: &R,
    ctx: &CompletionContext,
    items: Vec<CompletionItem>,
    timeout: Duration,
) -> Vec<CompletionItem> {
    let fallback = items.clone();

    let ranked_future = ranker.rank_completions(ctx, items);
    let ranked_future = std::panic::AssertUnwindSafe(ranked_future).catch_unwind();

    match util::timeout(timeout, ranked_future).await {
        Ok(Ok(ranked)) => ranked,
        Ok(Err(_panic)) => fallback,
        Err(_timeout) => fallback,
    }
}

/// Convenience helper for integrating ranking behind feature flags.
///
/// When disabled (or if ranking fails/times out), this returns `items` unchanged.
pub async fn maybe_rank_completions<R: CompletionRanker>(
    config: &AiConfig,
    ranker: &R,
    ctx: &CompletionContext,
    items: Vec<CompletionItem>,
) -> Vec<CompletionItem> {
    if !config.features.completion_ranking {
        return items;
    }

    rank_completions_with_timeout(ranker, ctx, items, config.timeouts.completion_ranking).await
}
