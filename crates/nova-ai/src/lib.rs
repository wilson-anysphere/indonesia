//! `nova-ai` provides AI-adjacent functionality for Project Nova.
//!
//! This crate is deliberately model-agnostic: it defines configuration and
//! interfaces, plus baseline non-ML implementations that allow other crates to
//! integrate AI enhancements behind feature flags.

mod anonymizer;
mod completion_ranking;
mod config;
mod semantic_search;
mod util;

pub use anonymizer::{CodeAnonymizer, CodeAnonymizerOptions};
pub use completion_ranking::{
    maybe_rank_completions, rank_completions_with_timeout, BaselineCompletionRanker,
    CompletionRanker,
};
pub use config::{
    AiConfig, AiFeatures, AiTimeouts, CloudConfig, CloudProvider, LocalModelConfig, PrivacyConfig,
};
pub use semantic_search::{SearchResult, SemanticSearch, TrigramSemanticSearch};

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use futures::executor::block_on;
    use futures::future::BoxFuture;
    use futures::FutureExt;

    use nova_core::ProjectDatabase;
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
        let config = AiConfig::disabled();
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

        let ranked = block_on(rank_completions_with_timeout(
            &ranker,
            &ctx,
            items.clone(),
            Duration::from_millis(1),
        ));

        assert_eq!(ranked, items);
    }

    #[test]
    fn ranking_panics_return_fallback() {
        struct PanicRanker;

        impl CompletionRanker for PanicRanker {
            fn rank_completions<'a>(
                &'a self,
                _ctx: &'a CompletionContext,
                _items: Vec<CompletionItem>,
            ) -> BoxFuture<'a, Vec<CompletionItem>> {
                async move { panic!("boom") }.boxed()
            }
        }

        let ranker = PanicRanker;
        let ctx = CompletionContext::new("p", "");
        let items = vec![
            CompletionItem::new("print", CompletionItemKind::Method),
            CompletionItem::new("println", CompletionItemKind::Method),
        ];

        let ranked = block_on(rank_completions_with_timeout(
            &ranker,
            &ctx,
            items.clone(),
            Duration::from_millis(20),
        ));

        assert_eq!(ranked, items);
    }

    #[test]
    fn trigram_search_finds_best_match() {
        #[derive(Debug)]
        struct MemDb(Vec<(PathBuf, String)>);

        impl ProjectDatabase for MemDb {
            fn project_files(&self) -> Vec<PathBuf> {
                self.0.iter().map(|(p, _)| p.clone()).collect()
            }

            fn file_text(&self, path: &Path) -> Option<String> {
                self.0
                    .iter()
                    .find(|(p, _)| p == path)
                    .map(|(_, text)| text.clone())
            }
        }

        let db = MemDb(vec![
            (
                PathBuf::from("src/A.java"),
                "class A { void helloWorld() {} }".into(),
            ),
            (
                PathBuf::from("src/B.java"),
                "class B { void goodbye() {} }".into(),
            ),
        ]);

        let mut search = TrigramSemanticSearch::new();
        search.index_project(&db);

        let results = search.search("helloWorld");
        assert!(!results.is_empty());
        assert_eq!(results[0].path, PathBuf::from("src/A.java"));
    }
}
