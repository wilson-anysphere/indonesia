use std::cmp::Ordering;
use std::sync::OnceLock;
use std::time::Duration;

use futures::future::{BoxFuture, FutureExt};

use nova_config::AiConfig;
use nova_core::{panic_payload_to_str, CompletionContext, CompletionItem, CompletionItemKind};
use nova_fuzzy::{FuzzyMatcher, MatchScore};

use crate::util;

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
    fn kind_bonus(kind: CompletionItemKind) -> i32 {
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
}

impl CompletionRanker for BaselineCompletionRanker {
    fn rank_completions<'a>(
        &'a self,
        ctx: &'a CompletionContext,
        mut items: Vec<CompletionItem>,
    ) -> BoxFuture<'a, Vec<CompletionItem>> {
        // Use the shared `nova-fuzzy` scorer so baseline ranking is consistent
        // with other non-AI fuzzy matching facilities in Nova.
        let mut matcher = FuzzyMatcher::new(&ctx.prefix);

        let mut scored: Vec<(CompletionItem, Option<MatchScore>, i32)> = items
            .drain(..)
            .map(|item| {
                let score = matcher.score(&item.label);
                let bonus = Self::kind_bonus(item.kind);
                (item, score, bonus)
            })
            .collect();

        scored.sort_by(|(a_item, a_score, a_bonus), (b_item, b_score, b_bonus)| {
            match (a_score, b_score) {
                (Some(a_score), Some(b_score)) => b_score
                    .rank_key()
                    .cmp(&a_score.rank_key())
                    .then_with(|| b_bonus.cmp(a_bonus))
                    .then_with(|| a_item.label.len().cmp(&b_item.label.len()))
                    .then_with(|| a_item.label.cmp(&b_item.label)),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => b_bonus
                    .cmp(a_bonus)
                    .then_with(|| a_item.label.len().cmp(&b_item.label.len()))
                    .then_with(|| a_item.label.cmp(&b_item.label)),
            }
        });

        let ranked: Vec<CompletionItem> = scored.into_iter().map(|(item, _, _)| item).collect();
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
    static RANKING_PANIC_LOGGED: OnceLock<()> = OnceLock::new();
    static RANKING_TIMEOUT_LOGGED: OnceLock<()> = OnceLock::new();

    let fallback = items.clone();

    let ranked_future = ranker.rank_completions(ctx, items);
    let ranked_future = std::panic::AssertUnwindSafe(ranked_future).catch_unwind();

    match util::timeout(timeout, ranked_future).await {
        Ok(Ok(ranked)) => ranked,
        Ok(Err(panic)) => {
            if RANKING_PANIC_LOGGED.set(()).is_ok() {
                tracing::error!(
                    target = "nova.ai",
                    ranker = std::any::type_name::<R>(),
                    panic = %panic_payload_to_str(&*panic),
                    prefix_len = ctx.prefix.len(),
                    timeout_ms = timeout.as_millis(),
                    "completion ranking panicked; returning unranked completions"
                );
            }
            fallback
        }
        Err(_timeout) => {
            if RANKING_TIMEOUT_LOGGED.set(()).is_ok() {
                tracing::debug!(
                    target = "nova.ai",
                    ranker = std::any::type_name::<R>(),
                    prefix_len = ctx.prefix.len(),
                    timeout_ms = timeout.as_millis(),
                    "completion ranking timed out; returning unranked completions"
                );
            }
            fallback
        }
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
    if !(config.enabled && config.features.completion_ranking) {
        return items;
    }

    rank_completions_with_timeout(ranker, ctx, items, config.timeouts.completion_ranking()).await
}
