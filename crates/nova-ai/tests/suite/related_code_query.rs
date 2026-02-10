use std::path::PathBuf;
use std::sync::Mutex;

use nova_ai::context::RELATED_CODE_QUERY_MAX_BYTES;
use nova_ai::{ContextRequest, PrivacyMode, SearchResult, SemanticSearch, TrigramSemanticSearch, VirtualWorkspace};

fn base_request(focal_code: &str) -> ContextRequest {
    ContextRequest {
        file_path: None,
        focal_code: focal_code.to_string(),
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
    }
}

#[test]
fn related_code_query_prefers_identifiers_over_comment_noise() {
    let db = VirtualWorkspace::new([
        (
            "src/UserRepository.java".to_string(),
            r#"
                package app.data;

                public class UserRepository {
                    public User findByEmail(String email) { return null; }
                }
            "#
            .to_string(),
        ),
        (
            "src/Noise.java".to_string(),
            r#"
                public class Noise {
                    /*
                    lorem ipsum dolor sit amet consectetur adipiscing elit
                    lorem ipsum dolor sit amet consectetur adipiscing elit
                    lorem ipsum dolor sit amet consectetur adipiscing elit
                    lorem ipsum dolor sit amet consectetur adipiscing elit
                    */
                    public void noop() {}
                }
            "#
            .to_string(),
        ),
    ]);

    let mut search = TrigramSemanticSearch::new();
    search.index_project(&db);

    let focal_code = r#"
        /*
        lorem ipsum dolor sit amet consectetur adipiscing elit
        lorem ipsum dolor sit amet consectetur adipiscing elit
        lorem ipsum dolor sit amet consectetur adipiscing elit
        lorem ipsum dolor sit amet consectetur adipiscing elit
        */
        return userRepository.findByEmail(email);
    "#;

    let req = base_request(focal_code).with_related_code_from_focal(&search, 1);
    assert_eq!(
        req.related_code.first().map(|c| c.path.clone()),
        Some(PathBuf::from("src/UserRepository.java")),
        "expected semantic-search enrichment to ignore comment noise and match the symbol-bearing file"
    );
}

#[test]
fn related_code_query_is_length_capped_and_path_safe() {
    #[derive(Default)]
    struct CapturingSearch {
        last_query: Mutex<Option<String>>,
    }

    impl SemanticSearch for CapturingSearch {
        fn search(&self, query: &str) -> Vec<SearchResult> {
            *self.last_query.lock().expect("lock poisoned") = Some(query.to_string());
            Vec::new()
        }
    }

    let search = CapturingSearch::default();
    let huge = "public void veryLongMethodNameWithLotsOfStuff() { int count = 0; }\n"
        .repeat(1024);
    let focal_code = format!(
        "{huge}\n// leaking path should not show up: /home/alice/project/secret.txt\n"
    );

    let _ = base_request(&focal_code).with_related_code_from_focal(&search, 1);
    let query = search
        .last_query
        .lock()
        .expect("lock poisoned")
        .clone()
        .expect("query captured");

    assert!(
        query.len() <= RELATED_CODE_QUERY_MAX_BYTES,
        "expected query length <= {RELATED_CODE_QUERY_MAX_BYTES}, got {}",
        query.len()
    );
    assert!(
        !query.contains("/home/alice/project/secret.txt"),
        "query should not include file paths: {query}"
    );
}

