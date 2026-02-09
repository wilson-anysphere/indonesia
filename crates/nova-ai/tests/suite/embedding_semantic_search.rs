#![cfg(feature = "embeddings")]

use std::path::PathBuf;

use nova_ai::{
    AiError, ContextBuilder, ContextRequest, Embedder, EmbeddingSemanticSearch, HashEmbedder,
    PrivacyMode,
    SemanticSearch, VirtualWorkspace,
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
