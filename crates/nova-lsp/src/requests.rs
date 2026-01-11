use lsp_types::CompletionItem;
use serde::{Deserialize, Serialize};

pub const NOVA_COMPLETION_MORE_METHOD: &str = "nova/completion/more";

/// Params for `nova/completion/more`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct MoreCompletionsParams {
    #[serde(alias = "contextId")]
    pub context_id: String,
}

/// Result for `nova/completion/more`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct MoreCompletionsResult {
    pub items: Vec<CompletionItem>,
    /// Mirrors LSP semantics: `true` means the client may poll again.
    pub is_incomplete: bool,
}
