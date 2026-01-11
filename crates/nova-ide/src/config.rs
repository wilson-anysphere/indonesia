use serde::{Deserialize, Serialize};

/// User-facing completion configuration.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompletionConfig {
    pub ai_enabled: bool,
    /// Maximum AI items that will be surfaced for a context.
    pub ai_max_items: usize,
    /// Maximum number of additional edits (imports) an AI item may request.
    pub ai_max_additional_edits: usize,
    /// Maximum token budget for the insert text (best-effort tokenization).
    pub ai_max_tokens: usize,
    /// Timeout (in milliseconds) for a single AI completion request.
    #[serde(default = "default_ai_timeout_ms")]
    pub ai_timeout_ms: u64,
}

fn default_ai_timeout_ms() -> u64 {
    5_000
}

impl Default for CompletionConfig {
    fn default() -> Self {
        Self {
            ai_enabled: true,
            ai_max_items: 8,
            ai_max_additional_edits: 3,
            ai_max_tokens: 64,
            ai_timeout_ms: default_ai_timeout_ms(),
        }
    }
}
