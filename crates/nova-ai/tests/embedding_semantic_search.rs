#![cfg(feature = "embeddings")]

use std::path::{Path, PathBuf};

use nova_ai::{
    ContextBuilder, ContextRequest, EmbeddingSemanticSearch, HashEmbedder, PrivacyMode,
    SemanticSearch,
};
use nova_core::ProjectDatabase;

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

#[test]
fn embedding_search_ranks_most_relevant_method_first() {
    let db = MemDb(vec![
        (
            PathBuf::from("src/Hello.java"),
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
            PathBuf::from("src/Other.java"),
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
    let db = MemDb(vec![
        (
            PathBuf::from("src/Hello.java"),
            "public class Hello { public String helloWorld() { return \"hello world\"; } }"
                .to_string(),
        ),
        (
            PathBuf::from("src/Other.java"),
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
