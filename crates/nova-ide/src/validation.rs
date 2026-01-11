use crate::CompletionConfig;
use nova_ai::{MultiTokenCompletion, MultiTokenCompletionContext};

/// Validate AI completions against best-effort semantic constraints.
pub fn validate_ai_completion(
    ctx: &MultiTokenCompletionContext,
    completion: &MultiTokenCompletion,
    config: &CompletionConfig,
) -> bool {
    nova_ai::validate_multi_token_completion(
        ctx,
        completion,
        config.ai_max_additional_edits,
        config.ai_max_tokens,
    )
}

