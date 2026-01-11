use crate::{validate_ai_completion, CompletionConfig, NovaCompletionItem};
use nova_ai::{
    CancellationToken, CompletionContextBuilder, MultiTokenCompletionContext,
    MultiTokenCompletionProvider, MultiTokenCompletionRequest,
};
use std::sync::Arc;
use tokio::time;

#[derive(Clone)]
pub struct CompletionEngine {
    config: CompletionConfig,
    context_builder: CompletionContextBuilder,
    ai_provider: Option<Arc<dyn MultiTokenCompletionProvider>>,
}

impl CompletionEngine {
    pub fn new(
        config: CompletionConfig,
        context_builder: CompletionContextBuilder,
        ai_provider: Option<Arc<dyn MultiTokenCompletionProvider>>,
    ) -> Self {
        Self {
            config,
            context_builder,
            ai_provider,
        }
    }

    pub fn config(&self) -> &CompletionConfig {
        &self.config
    }

    pub fn supports_ai(&self) -> bool {
        self.config.ai_enabled && self.ai_provider.is_some()
    }

    /// Synchronous semantic completions (fast path).
    pub fn standard_completions(
        &self,
        ctx: &MultiTokenCompletionContext,
    ) -> Vec<NovaCompletionItem> {
        ctx.available_methods
            .iter()
            .map(|name| NovaCompletionItem::standard(name, name))
            .collect()
    }

    /// AI multi-token completions (slow path).
    pub async fn ai_completions_async(
        &self,
        ctx: &MultiTokenCompletionContext,
        cancel: CancellationToken,
    ) -> Vec<NovaCompletionItem> {
        if !self.supports_ai() {
            return Vec::new();
        }

        if cancel.is_cancelled() {
            return Vec::new();
        }

        let provider = match &self.ai_provider {
            Some(provider) => Arc::clone(provider),
            None => return Vec::new(),
        };

        let prompt = self
            .context_builder
            .build_completion_prompt(ctx, self.config.ai_max_items);

        let timeout = std::time::Duration::from_millis(self.config.ai_timeout_ms.max(1));
        let request = MultiTokenCompletionRequest {
            prompt,
            max_items: self.config.ai_max_items,
            timeout,
            cancel: cancel.clone(),
        };

        let request = provider.complete_multi_token(request);
        tokio::pin!(request);

        let suggestions = match tokio::select! {
            res = time::timeout(timeout, &mut request) => match res {
                Ok(res) => res,
                Err(_) => {
                    cancel.cancel();
                    let _ = time::timeout(std::time::Duration::from_millis(250), &mut request).await;
                    Err(nova_ai::AiProviderError::Timeout)
                }
            },
            _ = cancel.cancelled() => {
                match time::timeout(std::time::Duration::from_millis(250), &mut request).await {
                    Ok(res) => res,
                    Err(_) => Err(nova_ai::AiProviderError::Cancelled),
                }
            }
        } {
            Ok(suggestions) => suggestions,
            Err(_err) => return Vec::new(),
        };

        if cancel.is_cancelled() {
            return Vec::new();
        }

        let mut items = Vec::new();
        for suggestion in suggestions {
            if !validate_ai_completion(ctx, &suggestion, &self.config) {
                continue;
            }

            items.push(NovaCompletionItem::ai(
                suggestion.label,
                suggestion.insert_text,
                suggestion.format,
                suggestion.additional_edits,
                suggestion.confidence,
            ));
        }

        // Rank by confidence descending for deterministic results.
        items.sort_by(|a, b| {
            let a_conf = a.confidence.unwrap_or(0.0);
            let b_conf = b.confidence.unwrap_or(0.0);
            b_conf
                .total_cmp(&a_conf)
                .then_with(|| a.label.cmp(&b.label))
        });

        if items.len() > self.config.ai_max_items {
            items.truncate(self.config.ai_max_items);
        }

        items
    }
}
