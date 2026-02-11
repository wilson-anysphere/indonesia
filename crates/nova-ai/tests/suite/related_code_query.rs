use std::path::PathBuf;
use std::sync::Mutex;

use nova_ai::context::RELATED_CODE_QUERY_MAX_BYTES;
use nova_ai::{
    ContextRequest, PrivacyMode, SearchResult, SemanticSearch, TrigramSemanticSearch,
    VirtualWorkspace,
};

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
fn related_code_query_is_length_capped() {
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
    let long_ident = "VeryLongIdentifierNameWithLotsOfRepeatingStuff".repeat(64);
    assert!(
        long_ident.len() > RELATED_CODE_QUERY_MAX_BYTES,
        "expected long identifier to exceed query cap"
    );

    let focal_code = format!("int {long_ident} = 0;");

    let _ = base_request(&focal_code).with_related_code_from_focal(&search, 1);
    let query = search
        .last_query
        .lock()
        .expect("lock poisoned")
        .clone()
        .expect("query captured");

    assert!(
        query.len() == RELATED_CODE_QUERY_MAX_BYTES,
        "expected query length == {RELATED_CODE_QUERY_MAX_BYTES}, got {}",
        query.len(),
    );
    assert_eq!(
        query,
        long_ident[..RELATED_CODE_QUERY_MAX_BYTES],
        "expected query to be truncated identifier prefix"
    );
}

#[test]
fn related_code_query_avoids_path_segments() {
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
    let private_segment = "NOVA_AI_PRIVATE_USER_12345";
    let focal_code = format!("/home/{private_segment}/project/secret.txt\nreturn foo.bar();\n");

    let _ = base_request(&focal_code).with_related_code_from_focal(&search, 1);
    let query = search
        .last_query
        .lock()
        .expect("lock poisoned")
        .clone()
        .expect("query captured");

    assert!(
        !query.contains(private_segment),
        "query should not include path segments: {query}"
    );
    assert!(
        !query.to_ascii_lowercase().contains("secret"),
        "query should not include file-name segments: {query}"
    );
    assert!(
        query.contains("foo") || query.contains("bar"),
        "expected query to retain non-path identifiers, got: {query}"
    );
}

#[test]
fn related_code_query_avoids_relative_path_segments_without_extensions() {
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
    let private_segment = "NOVA_AI_PRIVATE_USER_12345";
    let focal_code = format!("{private_segment}/credentials\nreturn foo.bar();\n");

    let _ = base_request(&focal_code).with_related_code_from_focal(&search, 1);
    let query = search
        .last_query
        .lock()
        .expect("lock poisoned")
        .clone()
        .expect("query captured");

    assert!(
        !query.contains(private_segment),
        "query should not include path segments: {query}"
    );
    assert!(
        !query.to_ascii_lowercase().contains("credentials"),
        "query should not include path segments: {query}"
    );
    assert!(
        query.contains("foo") || query.contains("bar"),
        "expected query to retain non-path identifiers, got: {query}"
    );
}

#[test]
fn related_code_query_avoids_path_segments_with_internal_punctuation() {
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
    let private_segment = "NOVA_AI_PRIVATE_USER_12345";
    let focal_code = format!(
        "/home/user/my-{private_segment}-project/src/Foo.java\nreturn foo.bar();\n"
    );

    let _ = base_request(&focal_code).with_related_code_from_focal(&search, 1);
    let query = search
        .last_query
        .lock()
        .expect("lock poisoned")
        .clone()
        .expect("query captured");

    assert!(
        !query.contains(private_segment),
        "query should not include internal path segment fragments: {query}"
    );
    assert!(
        query.contains("foo") || query.contains("bar"),
        "expected query to retain non-path identifiers, got: {query}"
    );
}

#[test]
fn related_code_query_does_not_drop_identifiers_due_to_inline_string_paths() {
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

    // A common code shape: identifiers + a string literal containing `/` (URL/path) in the same
    // whitespace token. The query heuristic should still extract the surrounding identifiers.
    let search = CapturingSearch::default();
    let focal_code = r#"return userRepository.findByPath("/home/private_user_123/project");"#;

    let _ = base_request(focal_code).with_related_code_from_focal(&search, 1);
    let query = search
        .last_query
        .lock()
        .expect("lock poisoned")
        .clone()
        .expect("query captured");

    assert!(
        query.contains("userRepository") || query.contains("findByPath"),
        "expected query to retain surrounding identifiers, got: {query}"
    );
    assert!(
        !query.contains("private_user_123"),
        "expected query to avoid path segments in string literal, got: {query}"
    );
}

#[test]
fn related_code_query_avoids_file_name_tokens_with_extensions() {
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
    let focal_code = r#"
        Secret-config.properties
        return foo.bar();
    "#;

    let _ = base_request(focal_code).with_related_code_from_focal(&search, 1);
    let query = search
        .last_query
        .lock()
        .expect("lock poisoned")
        .clone()
        .expect("query captured");

    assert!(
        !query.to_ascii_lowercase().contains("secret"),
        "query should not include file-name segments: {query}"
    );
    assert!(
        query.contains("foo") || query.contains("bar"),
        "expected query to retain non-file identifiers, got: {query}"
    );
}

#[test]
fn related_code_query_avoids_file_names_with_line_number_suffixes() {
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
    let focal_code = r#"
        Foo.java:123
        return foo.bar();
    "#;

    let _ = base_request(focal_code).with_related_code_from_focal(&search, 1);
    let query = search
        .last_query
        .lock()
        .expect("lock poisoned")
        .clone()
        .expect("query captured");

    assert!(
        !query.contains("Foo"),
        "query should not include file-name base segments: {query}"
    );
    assert!(
        !query.split_whitespace().any(|tok| tok.eq_ignore_ascii_case("java")),
        "query should not include file-name extension segments: {query}"
    );
    assert!(
        query.contains("foo") || query.contains("bar"),
        "expected query to retain code identifiers, got: {query}"
    );
}

#[test]
fn related_code_query_avoids_file_names_with_trailing_period() {
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
    let focal_code = r#"
        Please see Foo.java.
        return foo.bar();
    "#;

    let _ = base_request(focal_code).with_related_code_from_focal(&search, 1);
    let query = search
        .last_query
        .lock()
        .expect("lock poisoned")
        .clone()
        .expect("query captured");

    assert!(
        !query.contains("Foo"),
        "query should not include file-name base segments: {query}"
    );
    assert!(
        !query.split_whitespace().any(|tok| tok.eq_ignore_ascii_case("java")),
        "query should not include file-name extension segments: {query}"
    );
    assert!(
        query.contains("foo") || query.contains("bar"),
        "expected query to retain code identifiers, got: {query}"
    );
}

#[test]
fn related_code_query_skips_stacktrace_filename_only_selections() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for stacktrace filename-only selections");
        }
    }

    let search = PanicSearch;
    let focal_code = "Foo.java:123";
    let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
    assert!(
        req.related_code.is_empty(),
        "expected no related code for filename-only focal code"
    );
}

#[test]
fn related_code_query_skips_file_uri_only_selections() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for file-uri-only selections");
        }
    }

    let search = PanicSearch;
    let focal_code = "file:///home/user/project/src/Foo.java";
    let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
    assert!(
        req.related_code.is_empty(),
        "expected no related code for file-uri-only focal code"
    );
}

#[test]
fn related_code_query_skips_vscode_remote_uri_only_selections() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for vscode-remote uri-only selections");
        }
    }

    let search = PanicSearch;
    let focal_code = "vscode-remote://ssh-remote+myhost/home/user/project/src/Foo.java";
    let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
    assert!(
        req.related_code.is_empty(),
        "expected no related code for vscode-remote uri-only focal code"
    );
}

#[test]
fn related_code_query_does_not_treat_member_access_with_underscore_suffix_as_filename() {
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
    let focal_code = "return config._properties;";

    let _ = base_request(focal_code).with_related_code_from_focal(&search, 1);
    let query = search
        .last_query
        .lock()
        .expect("lock poisoned")
        .clone()
        .expect("query captured");

    assert!(
        query.contains("config") || query.contains("_properties"),
        "expected member-access identifiers to remain in query, got: {query}"
    );
}

#[test]
fn related_code_query_skips_obvious_secret_tokens_in_fallback() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for secret-like related-code queries");
        }
    }

    let search = PanicSearch;
    let focal_code = r#""sk-verysecretstringthatislong""#;
    let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
    assert!(
        req.related_code.is_empty(),
        "expected no related code for secret-like focal code"
    );
}

#[test]
fn related_code_query_skips_secret_values_embedded_in_json_tokens() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for secret-containing JSON tokens");
        }
    }

    let search = PanicSearch;
    let focal_code = r#""apiKey":"sk-verysecretstringthatislong""#;
    let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
    assert!(
        req.related_code.is_empty(),
        "expected no related code for secret-containing focal code"
    );
}

#[test]
fn related_code_query_skips_unquoted_secret_value_selections() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for unquoted secret-only selections");
        }
    }

    let search = PanicSearch;
    let focal_code = "sk-verysecretstringthatislong";
    let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
    assert!(
        req.related_code.is_empty(),
        "expected no related code for unquoted secret-like focal code"
    );
}

#[test]
fn related_code_query_skips_password_assignment_selections() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for password assignment selections");
        }
    }

    let search = PanicSearch;
    let focal_code = "password=hunter2";
    let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
    assert!(
        req.related_code.is_empty(),
        "expected no related code for password assignment focal code"
    );
}

#[test]
fn related_code_query_skips_password_colon_assignment_selections() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for password colon assignment selections");
        }
    }

    let search = PanicSearch;
    let focal_code = "password: hunter2";
    let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
    assert!(
        req.related_code.is_empty(),
        "expected no related code for password colon assignment focal code"
    );
}

#[test]
fn related_code_query_skips_json_password_colon_assignment_selections() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for JSON password colon assignment selections");
        }
    }

    let search = PanicSearch;
    let focal_code = r#""password":"hunter2""#;
    let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
    assert!(
        req.related_code.is_empty(),
        "expected no related code for JSON password colon assignment focal code"
    );
}

#[test]
fn related_code_query_skips_email_address_only_selections() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for email-only related-code queries");
        }
    }

    let search = PanicSearch;
    let focal_code = "alice@example.com";
    let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
    assert!(
        req.related_code.is_empty(),
        "expected no related code for email-only focal code"
    );
}

#[test]
fn related_code_query_skips_ipv4_only_selections() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for ip-only related-code queries");
        }
    }

    let search = PanicSearch;
    let focal_code = "192.168.0.1";
    let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
    assert!(
        req.related_code.is_empty(),
        "expected no related code for ip-only focal code"
    );
}

#[test]
fn related_code_query_skips_empty_queries() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for empty related-code queries");
        }
    }

    let search = PanicSearch;
    let req = base_request("").with_related_code_from_focal(&search, 3);
    assert!(
        req.related_code.is_empty(),
        "expected no related code for empty focal code"
    );

    let req = base_request("   \n\t").with_related_code_from_focal(&search, 3);
    assert!(
        req.related_code.is_empty(),
        "expected no related code for whitespace-only focal code"
    );

    // Explicitly verify the `max_results=0` early-return path as well.
    let req = base_request("something").with_related_code_from_focal(&search, 0);
    assert!(
        req.related_code.is_empty(),
        "expected no related code when max_results=0"
    );

    // The helper should clear any pre-populated related code when it skips search.
    let mut req = base_request("something");
    req.related_code.push(nova_ai::context::RelatedCode {
        path: PathBuf::from("src/Dummy.java"),
        range: 0..0,
        kind: "file".to_string(),
        snippet: "dummy".to_string(),
    });
    let req = req.with_related_code_from_search(&search, "", 3);
    assert!(
        req.related_code.is_empty(),
        "expected related code to be cleared when search is skipped"
    );
}

#[test]
fn related_code_query_skips_low_signal_focal_code() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for low-signal related-code queries");
        }
    }

    let search = PanicSearch;
    let req = base_request("return a + b;").with_related_code_from_focal(&search, 3);
    assert!(
        req.related_code.is_empty(),
        "expected no related code for low-signal focal code"
    );

    let req = base_request("null").with_related_code_from_focal(&search, 3);
    assert!(
        req.related_code.is_empty(),
        "expected no related code for stop-word-only focal code"
    );
}

#[test]
fn related_code_query_ignores_java_text_blocks() {
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
    let focal_code = r#"
        String sql = """
          SELECT user_id FROM users WHERE email = ?
        """;
        return userRepository.findByEmail(email);
    "#;

    let _ = base_request(focal_code).with_related_code_from_focal(&search, 1);
    let query = search
        .last_query
        .lock()
        .expect("lock poisoned")
        .clone()
        .expect("query captured");

    assert!(
        !query.contains("SELECT") && !query.to_ascii_lowercase().contains("users"),
        "expected query to ignore text block contents, got: {query}"
    );
    assert!(
        query.contains("userRepository") || query.contains("findByEmail"),
        "expected query to retain identifier tokens, got: {query}"
    );
}

#[test]
fn related_code_query_drops_single_letter_type_params() {
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
    let focal_code = r#"
        public <T> T map(T value) {
            return mapper.map(value);
        }
    "#;

    let _ = base_request(focal_code).with_related_code_from_focal(&search, 1);
    let query = search
        .last_query
        .lock()
        .expect("lock poisoned")
        .clone()
        .expect("query captured");

    assert!(
        !query.split_whitespace().any(|tok| tok == "T"),
        "expected query to exclude generic type parameter `T`, got: {query}"
    );
    assert!(
        query.contains("mapper") || query.contains("map"),
        "expected query to retain identifier tokens, got: {query}"
    );
}
