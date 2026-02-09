#![cfg(feature = "embeddings")]

use std::path::PathBuf;

use nova_ai::{
    AiError, ContextBuilder, ContextRequest, Embedder, EmbeddingSemanticSearch, HashEmbedder,
    PrivacyMode, SemanticSearch, VirtualWorkspace,
};

#[test]
fn embedding_search_ranks_most_relevant_method_first() {
    let db = VirtualWorkspace::new([
        (
            "src/Hello.java".to_string(),
            r#"
                package com.example;

                public class Hello {
                    /** Says hello world. */
                    public String helloWorld() {
                        return "hello world";
                    }
                }
            "#
            .to_string(),
        ),
        (
            "src/Other.java".to_string(),
            r#"
                public class Other {
                    public String goodbye() {
                        return "goodbye";
                    }
                }
            "#
            .to_string(),
        ),
    ]);

    let mut search = EmbeddingSemanticSearch::new(HashEmbedder::default());
    search.index_project(&db);

    let results = search.search("hello world");
    assert!(!results.is_empty());
    assert_eq!(results[0].path, PathBuf::from("src/Hello.java"));
    assert_eq!(results[0].kind, "method");
}

#[test]
fn context_builder_can_include_embedding_related_code() {
    let db = VirtualWorkspace::new([
        (
            "src/Hello.java".to_string(),
            "public class Hello { public String helloWorld() { return \"hello world\"; } }"
                .to_string(),
        ),
        (
            "src/Other.java".to_string(),
            "public class Other { public String goodbye() { return \"goodbye\"; } }".to_string(),
        ),
    ]);

    let mut search = EmbeddingSemanticSearch::new(HashEmbedder::default());
    search.index_project(&db);

    let req = ContextRequest {
        file_path: None,
        focal_code: "return \"hello world\";".to_string(),
        enclosing_context: None,
        project_context: None,
        semantic_context: None,
        related_symbols: Vec::new(),
        related_code: Vec::new(),
        cursor: None,
        diagnostics: Vec::new(),
        extra_files: Vec::new(),
        doc_comments: None,
        include_doc_comments: false,
        token_budget: 200,
        privacy: PrivacyMode {
            anonymize_identifiers: false,
            include_file_paths: true,
            ..PrivacyMode::default()
        },
    };

    let ctx = ContextBuilder::new().build_with_semantic_search(req, &search, 1);
    assert!(ctx.text.contains("## Related code"));
    assert!(ctx.text.contains("helloWorld"));
}

#[test]
fn embedding_search_supports_incremental_indexing() {
    let mut search = EmbeddingSemanticSearch::new(HashEmbedder::default());
    let path = PathBuf::from("src/Hello.java");

    search.index_file(
        path.clone(),
        r#"
            public class Hello {
                public String helloWorld() {
                    return "hello world";
                }
            }
        "#
        .to_string(),
    );

    let first = search.search("hello world");
    assert!(!first.is_empty());
    assert_eq!(first[0].path, path);
    assert!(first[0].snippet.contains("helloWorld"));

    search.index_file(
        path.clone(),
        r#"
            public class Hello {
                public String greetings() {
                    return "hello world";
                }
            }
        "#
        .to_string(),
    );

    let second = search.search("hello world");
    assert!(!second.is_empty());
    assert_eq!(second[0].path, path);
    assert!(second[0].snippet.contains("greetings"));
    assert_ne!(first[0].snippet, second[0].snippet);

    search.remove_file(path.as_path());
    assert!(search.search("hello world").is_empty());
}

#[test]
fn embedding_search_boosts_exact_substring_matches() {
    let db = VirtualWorkspace::new([(
        "src/Multi.java".to_string(),
        r#"
            package com.example;

            public class First {
                public String helloWorld() {
                    return "hello  world";
                }
            }

            public class Second {
                public String helloWorld() {
                    return "hello world";
                }
            }
        "#
        .to_string(),
    )]);

    let mut search = EmbeddingSemanticSearch::new(HashEmbedder::default());
    search.index_project(&db);

    let results = search.search("HELLO WORLD");
    assert_eq!(results.len(), 2);
    assert!(results[0].snippet.contains("hello world"));
    assert!(results[1].snippet.contains("hello  world"));
}

#[test]
fn embedding_search_skips_failed_doc_embeddings() {
    #[derive(Debug, Clone)]
    struct SelectiveFailEmbedder {
        inner: HashEmbedder,
    }

    impl Embedder for SelectiveFailEmbedder {
        fn embed(&self, text: &str) -> Result<Vec<f32>, AiError> {
            if text.contains("return \"goodbye\"") {
                return Err(AiError::UnexpectedResponse(
                    "forced embedding failure".to_string(),
                ));
            }
            self.inner.embed(text)
        }
    }

    let mut search = EmbeddingSemanticSearch::new(SelectiveFailEmbedder {
        inner: HashEmbedder::default(),
    });

    // Two methods ensures `EmbeddingSemanticSearch` uses `embed_batch` internally.
    search.index_file(
        PathBuf::from("src/Hello.java"),
        r#"
            public class Hello {
                public String helloWorld() {
                    return "hello world";
                }

                public String goodbye() {
                    return "goodbye";
                }
            }
        "#
        .to_string(),
    );

    let results = search.search("hello world");
    assert!(!results.is_empty());
    assert_eq!(results[0].kind, "method");
    assert!(results[0].snippet.contains("helloWorld"));

    // The failing doc should be absent from the index.
    let results = search.search("goodbye");
    assert!(!results.is_empty());
    assert!(!results[0].snippet.contains("goodbye"));
}

#[test]
fn embedding_search_returns_empty_when_query_embedding_fails() {
    #[derive(Debug, Clone)]
    struct QueryFailEmbedder {
        inner: HashEmbedder,
    }

    impl Embedder for QueryFailEmbedder {
        fn embed(&self, text: &str) -> Result<Vec<f32>, AiError> {
            if text == "boom" {
                return Err(AiError::UnexpectedResponse(
                    "forced query embedding failure".to_string(),
                ));
            }
            self.inner.embed(text)
        }
    }

    let mut search = EmbeddingSemanticSearch::new(QueryFailEmbedder {
        inner: HashEmbedder::default(),
    });
    search.index_file(
        PathBuf::from("src/Hello.java"),
        "public class Hello { public String helloWorld() { return \"hello world\"; } }"
            .to_string(),
    );

    assert!(search.search("boom").is_empty());
}

#[test]
fn embedding_search_chunks_non_java_files_with_aligned_ranges() {
    let mut readme = String::new();
    readme.push_str("# Example Project\n\n");

    // Add enough text to exceed the chunking threshold while mixing in multi-byte UTF-8
    // characters (Δ) so incorrect byte slicing would show up in range validation.
    for idx in 0..80 {
        readme.push_str(&format!(
            "Section {idx}: This is filler text with unicode Δ to grow the README for chunking.\n"
        ));
    }

    readme.push_str("\nNeedle phrase: flibbertigibbet quizzaciously\n\n");

    for idx in 0..80 {
        readme.push_str(&format!(
            "More filler {idx}: Additional markdown-ish text with unicode Δ.\n"
        ));
    }

    let mut search = EmbeddingSemanticSearch::new(HashEmbedder::default());
    let path = PathBuf::from("README.md");
    search.index_file(path.clone(), readme.clone());

    let results = search.search("flibbertigibbet quizzaciously");
    assert!(
        results.iter().any(|result| result.kind == "chunk"),
        "expected at least one chunk result, got: {results:?}"
    );

    let chunk = results
        .iter()
        .find(|result| result.kind == "chunk")
        .expect("chunk result missing");

    assert_eq!(chunk.path, path);
    assert!(readme.is_char_boundary(chunk.range.start));
    assert!(readme.is_char_boundary(chunk.range.end));
    assert_eq!(&readme[chunk.range.clone()], chunk.snippet);
    assert!(chunk.snippet.contains("flibbertigibbet"));
    assert!(chunk.range.end.saturating_sub(chunk.range.start) < readme.len());
}

#[test]
fn embedding_search_truncates_when_memory_budget_is_too_small() {
    let path = PathBuf::from("src/Hello.java");
    let text = r#"
        public class Hello {
            public String helloWorld() { return "hello world"; }
            public String helloAgain() { return "hello world"; }
            public String helloThird() { return "hello world"; }
        }
    "#
    .to_string();

    // Baseline: without a budget, we should index all method docs.
    let mut unlimited = EmbeddingSemanticSearch::new(HashEmbedder::default());
    unlimited.index_file(path.clone(), text.clone());
    let results = unlimited.search("hello world");
    assert_eq!(results.len(), 3, "expected all methods to be indexed");

    // Each method doc stores a `Vec<f32>` embedding. Use the embedder output length to compute a
    // tiny budget that can only hold two docs.
    let dims = HashEmbedder::default()
        .embed("hello")
        .expect("hash embedding")
        .len();
    let bytes_per_doc = dims * std::mem::size_of::<f32>();
    let max_memory_bytes = bytes_per_doc * 2;

    let mut limited = EmbeddingSemanticSearch::new(HashEmbedder::default())
        .with_max_memory_bytes(max_memory_bytes);
    limited.index_file(path, text);

    let limited_results = limited.search("hello world");
    assert!(
        !limited_results.is_empty(),
        "search should still work even when the index is truncated"
    );
    assert_eq!(limited_results.len(), 2, "expected truncation to two docs");
    assert!(limited_results.iter().any(|r| r.snippet.contains("helloWorld")));
    assert!(limited_results.iter().any(|r| r.snippet.contains("helloAgain")));
    assert!(!limited_results.iter().any(|r| r.snippet.contains("helloThird")));
}
