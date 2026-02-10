//! `nova-ai` provides AI-adjacent functionality for Project Nova.
//!
//! The crate is deliberately model-agnostic and designed for graceful
//! degradation:
//! - Local-only helpers like completion ranking and semantic search
//! - Privacy utilities (anonymization/redaction) for cloud integrations
//! - Optional cloud LLM client and higher-level AI actions

mod actions;
mod anonymizer;
mod audit;
mod cache;
mod client;
mod cloud;
mod code_edit_policy;
mod completion;
mod completion_context;
mod completion_filter;
mod completion_provider;
mod completion_ranking;
mod completion_ranking_prompt;
mod completion_validation;
#[doc(hidden)]
pub mod diff;
mod error;
mod http;
#[cfg(feature = "embeddings")]
mod embedders;
mod features;
mod llm_privacy;
mod project_database;
mod providers;
mod semantic_search;
mod stream_decode;
mod types;
mod util;

#[cfg(feature = "embeddings")]
pub mod embeddings;

pub mod cancel;
pub mod context;
pub mod patch;
pub mod privacy;
pub mod provider;
pub mod safety;
pub mod workspace;

pub use anonymizer::{
    deanonymize_additional_edit, deanonymize_java_like_code, deanonymize_multi_token_completion,
    CodeAnonymizer, CodeAnonymizerOptions,
};
pub use client::{AiClient, LlmClient};
pub use code_edit_policy::{enforce_code_edit_policy, CodeEditPolicyError};
pub use completion::{AdditionalEdit, MultiTokenCompletion, MultiTokenInsertTextFormat};
pub use completion_context::{CompletionContextBuilder, MultiTokenCompletionContext};
pub use completion_filter::filter_duplicates_against_insert_text_set;
pub use completion_provider::CloudMultiTokenCompletionProvider;
pub use completion_ranking::{
    maybe_rank_completions, rank_completions_with_timeout, BaselineCompletionRanker,
    CompletionRanker, LlmCompletionRanker,
};
pub use completion_ranking_prompt::CompletionRankingPromptBuilder;
pub use completion_validation::validate_multi_token_completion;
pub use context::{
    BuiltContext, ContextBuilder, ContextDiagnostic, ContextDiagnosticKind,
    ContextDiagnosticSeverity, ContextRequest, ContextSectionStat, RelatedCode, RelatedSymbol,
    SemanticContextBuilder,
};
pub use error::AiError;
pub use features::NovaAi;
pub use llm_privacy::ExcludedPathMatcher;
pub use actions::explain_error_prompt;
pub use privacy::{PrivacyMode, RedactionConfig};
pub use provider::{MultiTokenCompletionProvider, MultiTokenCompletionRequest};
pub use semantic_search::{
    semantic_search_from_config, NoopSemanticSearch, SearchResult, SemanticSearch,
    TrigramSemanticSearch,
};
#[cfg(feature = "embeddings")]
pub use semantic_search::{Embedder, EmbeddingSemanticSearch, HashEmbedder};
#[cfg(feature = "embeddings")]
pub use embedders::{AzureOpenAiEmbedder, OllamaEmbedder, OpenAiCompatibleEmbedder};
#[cfg(all(feature = "embeddings", feature = "embeddings-local"))]
pub use semantic_search::LocalEmbedder;
pub use types::{AiStream, ChatMessage, ChatRequest, ChatRole, CodeSnippet};

pub use project_database::DbProjectDatabase;
pub use project_database::SourceDbProjectDatabase;

pub use cancel::CancellationToken;
pub use patch::{parse_structured_patch, Patch, PatchParseError, TextEdit};
pub use provider::{AiProvider, AiProviderError};
pub use safety::{PatchSafetyConfig, SafetyError};
pub use workspace::{AppliedPatch, PatchApplyError, VirtualWorkspace};

/// Returns `true` when `nova-ai` was built with embedding-backed semantic search support.
///
/// This is a compile-time capability check that downstream crates (like `nova-lsp`) can use to
/// decide whether expensive embedding generation is possible in the current binary.
pub fn embeddings_available() -> bool {
    cfg!(feature = "embeddings")
}

/// Exposes diff filtering for integration tests.
///
/// This is `doc(hidden)` to avoid encouraging external callers to rely on the exact parsing
/// behavior (it is intentionally lightweight and may evolve).
#[doc(hidden)]
pub fn filter_diff_for_excluded_paths_for_tests<F>(diff: &str, is_excluded: F) -> String
where
    F: Fn(&std::path::Path) -> bool,
{
    diff::filter_diff_for_excluded_paths(diff, is_excluded).text
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::{Mutex, OnceLock};

    pub(crate) fn metrics_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use futures::executor::block_on;
    use futures::future::BoxFuture;
    use futures::FutureExt;

    use nova_core::{CompletionContext, CompletionItem, CompletionItemKind};

    use super::*;

    #[test]
    fn anonymizer_is_deterministic_within_session() {
        let code = r#"
            class MyClass {
                private String secretToken = "sk-012345678901234567890123456789";
                void foo(int userId) {
                    System.out.println(userId);
                }
            }
        "#;

        let mut anonymizer = CodeAnonymizer::new(CodeAnonymizerOptions {
            anonymize_identifiers: true,
            redact_sensitive_strings: true,
            redact_numeric_literals: true,
            strip_or_redact_comments: false,
        });

        let out1 = anonymizer.anonymize(code);
        let out2 = anonymizer.anonymize(code);

        assert_eq!(
            out1, out2,
            "anonymization should be deterministic per session"
        );
        assert!(out1.contains("String"));
        assert!(out1.contains("System"));
        assert!(out1.contains("println"));
        assert!(out1.contains("\"[REDACTED]\""));
        assert!(!out1.contains("MyClass"));
        assert!(!out1.contains("secretToken"));
    }

    #[test]
    fn anonymizer_preserves_stdlib_qualified_names() {
        let code = r#"
            import java.util.List;

            class Foo {
                java.util.List<String> list = null;
            }
        "#;

        let mut anonymizer = CodeAnonymizer::new(CodeAnonymizerOptions {
            anonymize_identifiers: true,
            redact_sensitive_strings: true,
            redact_numeric_literals: true,
            strip_or_redact_comments: false,
        });

        let out = anonymizer.anonymize(code);
        assert!(out.contains("java.util.List"));
        assert!(out.contains("String"));
        assert!(!out.contains("Foo"));
    }

    #[test]
    fn ranking_is_deterministic() {
        let ranker = BaselineCompletionRanker;
        let ctx = CompletionContext::new("pri", "");
        let items = vec![
            CompletionItem::new("private", CompletionItemKind::Keyword),
            CompletionItem::new("println", CompletionItemKind::Method),
            CompletionItem::new("print", CompletionItemKind::Method),
            CompletionItem::new("priority", CompletionItemKind::Variable),
        ];

        let ranked1 = block_on(ranker.rank_completions(&ctx, items.clone()));
        let ranked2 = block_on(ranker.rank_completions(&ctx, items.clone()));

        assert_eq!(ranked1, ranked2);
        assert_eq!(ranked1.first().unwrap().label, "print");
    }

    #[test]
    fn ranking_gracefully_degrades_when_disabled() {
        let config = nova_config::AiConfig::default();
        let ranker = BaselineCompletionRanker;
        let ctx = CompletionContext::new("pri", "");

        // Deliberately unsorted input: the baseline ranker would reorder this.
        let items = vec![
            CompletionItem::new("private", CompletionItemKind::Keyword),
            CompletionItem::new("print", CompletionItemKind::Method),
        ];

        let ranked = block_on(maybe_rank_completions(
            &config,
            &ranker,
            &ctx,
            items.clone(),
        ));
        assert_eq!(ranked, items);
    }

    #[test]
    fn ranking_times_out_returns_fallback() {
        let _guard = crate::test_support::metrics_lock()
            .lock()
            .expect("metrics lock poisoned");
        struct SlowRanker;

        impl CompletionRanker for SlowRanker {
            fn rank_completions<'a>(
                &'a self,
                _ctx: &'a CompletionContext,
                items: Vec<CompletionItem>,
            ) -> BoxFuture<'a, Vec<CompletionItem>> {
                async move {
                    futures_timer::Delay::new(Duration::from_millis(50)).await;
                    let mut items = items;
                    items.reverse();
                    items
                }
                .boxed()
            }
        }

        let ranker = SlowRanker;
        let ctx = CompletionContext::new("p", "");
        let items = vec![
            CompletionItem::new("print", CompletionItemKind::Method),
            CompletionItem::new("println", CompletionItemKind::Method),
        ];
        let metrics = nova_metrics::MetricsRegistry::global();
        let before_snapshot = metrics.snapshot();
        let before_timeouts = before_snapshot
            .methods
            .get(crate::completion_ranking::AI_COMPLETION_RANKING_METRIC)
            .map(|m| m.timeout_count)
            .unwrap_or(0);
        let before_requests = before_snapshot
            .methods
            .get(crate::completion_ranking::AI_COMPLETION_RANKING_METRIC)
            .map(|m| m.request_count)
            .unwrap_or(0);

        let ranked = block_on(rank_completions_with_timeout(
            &ranker,
            &ctx,
            items.clone(),
            Duration::from_millis(1),
        ));

        assert_eq!(ranked, items);

        let after_snapshot = metrics.snapshot();
        let after_timeouts = after_snapshot
            .methods
            .get(crate::completion_ranking::AI_COMPLETION_RANKING_METRIC)
            .map(|m| m.timeout_count)
            .unwrap_or(0);
        let after_requests = after_snapshot
            .methods
            .get(crate::completion_ranking::AI_COMPLETION_RANKING_METRIC)
            .map(|m| m.request_count)
            .unwrap_or(0);
        assert!(
            after_timeouts >= before_timeouts.saturating_add(1),
            "expected ai/completion_ranking timeout_count to increment"
        );
        assert!(
            after_requests >= before_requests.saturating_add(1),
            "expected ai/completion_ranking request_count to increment"
        );
    }

    #[test]
    fn ranking_panics_return_fallback() {
        let _guard = crate::test_support::metrics_lock()
            .lock()
            .expect("metrics lock poisoned");
        struct PanicRanker;

        impl CompletionRanker for PanicRanker {
            fn rank_completions<'a>(
                &'a self,
                _ctx: &'a CompletionContext,
                _items: Vec<CompletionItem>,
            ) -> BoxFuture<'a, Vec<CompletionItem>> {
                panic!("boom")
            }
        }

        let ranker = PanicRanker;
        let ctx = CompletionContext::new("p", "");
        let items = vec![
            CompletionItem::new("print", CompletionItemKind::Method),
            CompletionItem::new("println", CompletionItemKind::Method),
        ];
        let metrics = nova_metrics::MetricsRegistry::global();
        let before_snapshot = metrics.snapshot();
        let before_panics = before_snapshot
            .methods
            .get(crate::completion_ranking::AI_COMPLETION_RANKING_METRIC)
            .map(|m| m.panic_count)
            .unwrap_or(0);
        let before_requests = before_snapshot
            .methods
            .get(crate::completion_ranking::AI_COMPLETION_RANKING_METRIC)
            .map(|m| m.request_count)
            .unwrap_or(0);

        let ranked = block_on(rank_completions_with_timeout(
            &ranker,
            &ctx,
            items.clone(),
            Duration::from_millis(20),
        ));

        assert_eq!(ranked, items);

        let after_snapshot = metrics.snapshot();
        let after_panics = after_snapshot
            .methods
            .get(crate::completion_ranking::AI_COMPLETION_RANKING_METRIC)
            .map(|m| m.panic_count)
            .unwrap_or(0);
        let after_requests = after_snapshot
            .methods
            .get(crate::completion_ranking::AI_COMPLETION_RANKING_METRIC)
            .map(|m| m.request_count)
            .unwrap_or(0);
        assert!(
            after_panics >= before_panics.saturating_add(1),
            "expected ai/completion_ranking panic_count to increment"
        );
        assert!(
            after_requests >= before_requests.saturating_add(1),
            "expected ai/completion_ranking request_count to increment"
        );
    }

    #[test]
    fn trigram_search_finds_best_match() {
        let db = VirtualWorkspace::new([
            (
                "src/A.java".to_string(),
                "class A { void helloWorld() {} }".to_string(),
            ),
            (
                "src/B.java".to_string(),
                "class B { void goodbye() {} }".to_string(),
            ),
        ]);

        let mut search = TrigramSemanticSearch::new();
        search.index_project(&db);

        let results = search.search("helloWorld");
        assert!(!results.is_empty());
        assert_eq!(results[0].path, PathBuf::from("src/A.java"));
    }

    #[test]
    fn semantic_search_records_metrics() {
        let _guard = crate::test_support::metrics_lock()
            .lock()
            .expect("metrics lock poisoned");
        let metrics = nova_metrics::MetricsRegistry::global();

        let before_snapshot = metrics.snapshot();
        let before_index = before_snapshot
            .methods
            .get("ai/semantic_search/index_file")
            .map(|m| m.request_count)
            .unwrap_or(0);
        let before_search = before_snapshot
            .methods
            .get("ai/semantic_search/search")
            .map(|m| m.request_count)
            .unwrap_or(0);

        let db = VirtualWorkspace::new([
            ("src/A.java".to_string(), "class A { void hello() {} }".to_string()),
            ("src/B.java".to_string(), "class B { void goodbye() {} }".to_string()),
        ]);

        let mut search = TrigramSemanticSearch::new();
        search.index_project(&db);
        let _results = search.search("hello");

        let after_snapshot = metrics.snapshot();
        let after_index = after_snapshot
            .methods
            .get("ai/semantic_search/index_file")
            .map(|m| m.request_count)
            .unwrap_or(0);
        let after_search = after_snapshot
            .methods
            .get("ai/semantic_search/search")
            .map(|m| m.request_count)
            .unwrap_or(0);

        assert!(
            after_index >= before_index.saturating_add(2),
            "expected ai/semantic_search/index_file to record at least one update per indexed file"
        );
        assert!(
            after_search >= before_search.saturating_add(1),
            "expected ai/semantic_search/search to record requests"
        );
    }
}
