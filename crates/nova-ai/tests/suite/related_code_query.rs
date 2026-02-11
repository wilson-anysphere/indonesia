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
fn related_code_query_avoids_percent_encoded_path_segments() {
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
        "%2Fhome%2Fuser%2Fmy-{private_segment}-project%2Fsrc%2Fmain%2Fjava\nreturn foo.bar();\n"
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
        "query should not include percent-encoded path fragments: {query}"
    );
    assert!(
        query.contains("foo") || query.contains("bar"),
        "expected query to retain non-path identifiers, got: {query}"
    );
}

#[test]
fn related_code_query_avoids_unicode_escaped_path_segments() {
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
        "\\u002Fhome\\u002Fuser\\u002Fmy-{private_segment}-project\\u002Fsrc\\u002Fmain\\u002Fjava\nreturn foo.bar();\n"
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
        "query should not include unicode-escaped path fragments: {query}"
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
fn related_code_query_does_not_skip_due_to_sensitive_words_inside_string_literals() {
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
    let focal_code = r#"return userRepository.findByNote("password: hunter2");"#;

    let _ = base_request(focal_code).with_related_code_from_focal(&search, 1);
    let query = search
        .last_query
        .lock()
        .expect("lock poisoned")
        .clone()
        .expect("query captured");

    assert!(
        query.contains("userRepository") || query.contains("findByNote"),
        "expected query to retain surrounding identifiers, got: {query}"
    );
    assert!(
        !query.to_ascii_lowercase().contains("password"),
        "expected query to ignore string-literal contents, got: {query}"
    );
    assert!(
        !query.to_ascii_lowercase().contains("hunter2"),
        "expected query to ignore string-literal contents, got: {query}"
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
fn related_code_query_skips_domain_only_selections() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for domain-only selections");
        }
    }

    let search = PanicSearch;
    for focal_code in [r#""service.internal""#, r#""example.com""#] {
        let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
        assert!(
            req.related_code.is_empty(),
            "expected no related code for domain-only focal code"
        );
    }
}

#[test]
fn related_code_query_skips_percent_encoded_path_only_selections() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for percent-encoded path selections");
        }
    }

    let search = PanicSearch;
    for focal_code in [
        "%2Fhome%2Fuser%2Fsecret%2Fcredentials",
        "%5Chome%5Cuser%5Csecret%5Ccredentials",
        "%252Fhome%252Fuser%252Fsecret%252Fcredentials",
        "%255Chome%255Cuser%255Csecret%255Ccredentials",
    ] {
        let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
        assert!(
            req.related_code.is_empty(),
            "expected no related code for percent-encoded path-only focal code"
        );
    }
}

#[test]
fn related_code_query_skips_unicode_escaped_path_only_selections() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for unicode-escaped path selections");
        }
    }

    let search = PanicSearch;
    for focal_code in [
        r"\u002Fhome\u002Fuser\u002Fsecret\u002Fcredentials",
        r"\u005Chome\u005Cuser\u005Csecret\u005Ccredentials",
    ] {
        let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
        assert!(
            req.related_code.is_empty(),
            "expected no related code for unicode-escaped path-only focal code"
        );
    }
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
fn related_code_query_skips_user_at_host_only_selections() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for user@host related-code queries");
        }
    }

    let search = PanicSearch;
    let focal_code = "alice@localhost";
    let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
    assert!(
        req.related_code.is_empty(),
        "expected no related code for user@host-only focal code"
    );
}

#[test]
fn related_code_query_skips_user_at_host_port_only_selections() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for user@host:port related-code queries");
        }
    }

    let search = PanicSearch;
    let focal_code = "alice@localhost:8080";
    let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
    assert!(
        req.related_code.is_empty(),
        "expected no related code for user@host:port-only focal code"
    );
}

#[test]
fn related_code_query_skips_user_at_bracketed_ipv6_only_selections() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for user@[ipv6] related-code queries");
        }
    }

    let search = PanicSearch;
    let focal_code = "alice@[2001:db8::1]:22";
    let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
    assert!(
        req.related_code.is_empty(),
        "expected no related code for user@[ipv6]-only focal code"
    );
}

#[test]
fn related_code_query_skips_ipv6_only_selections() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for ipv6-only related-code queries");
        }
    }

    let search = PanicSearch;
    let focal_code = "2001:db8::1";
    let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
    assert!(
        req.related_code.is_empty(),
        "expected no related code for ipv6-only focal code"
    );
}

#[test]
fn related_code_query_skips_host_port_only_selections() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for host:port-only related-code queries");
        }
    }

    let search = PanicSearch;
    for focal_code in ["localhost:8080", "prod-host:8080", "service.internal:8080"] {
        let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
        assert!(
            req.related_code.is_empty(),
            "expected no related code for host:port-only focal code"
        );
    }
}

#[test]
fn related_code_query_skips_mac_address_only_selections() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for mac-address-only related-code queries");
        }
    }

    let search = PanicSearch;
    for focal_code in ["de:ad:be:ef:00:01", "de-ad-be-ef-00-01"] {
        let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
        assert!(
            req.related_code.is_empty(),
            "expected no related code for mac-only focal code"
        );
    }
}

#[test]
fn related_code_query_skips_delimited_number_only_selections() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for delimited-number-only related-code queries");
        }
    }

    let search = PanicSearch;
    for focal_code in ["123-45-6789", "+1-202-555-0143"] {
        let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
        assert!(
            req.related_code.is_empty(),
            "expected no related code for delimited-number-only focal code"
        );
    }
}

#[test]
fn related_code_query_skips_iso8601_timestamp_only_selections() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for timestamp-only related-code queries");
        }
    }

    let search = PanicSearch;
    let focal_code = "2026-02-11T12:34:56.789Z";
    let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
    assert!(
        req.related_code.is_empty(),
        "expected no related code for timestamp-only focal code"
    );
}

#[test]
fn related_code_query_skips_time_of_day_only_selections() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for time-only related-code queries");
        }
    }

    let search = PanicSearch;
    for focal_code in ["12:34:56", "12:34"] {
        let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
        assert!(
            req.related_code.is_empty(),
            "expected no related code for time-only focal code"
        );
    }
}

#[test]
fn related_code_query_skips_uuid_only_selections() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for uuid-only related-code queries");
        }
    }

    let search = PanicSearch;
    let focal_code = "de305d54-75b4-431b-adb2-eb6b9e546014";
    let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
    assert!(
        req.related_code.is_empty(),
        "expected no related code for uuid-only focal code"
    );
}

#[test]
fn related_code_query_skips_jwt_only_selections() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for jwt-only related-code queries");
        }
    }

    let search = PanicSearch;
    let focal_code = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4gRG9lIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
    let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
    assert!(
        req.related_code.is_empty(),
        "expected no related code for jwt-only focal code"
    );
}

#[test]
fn related_code_query_skips_base64_only_selections() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for base64-only related-code queries");
        }
    }

    let search = PanicSearch;
    let focal_code = "YWJjZGVmZ2hpamtsbW5vcHFyc3R1dnd4eXo=";
    let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
    assert!(
        req.related_code.is_empty(),
        "expected no related code for base64-only focal code"
    );
}

#[test]
fn related_code_query_skips_google_api_key_only_selections() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for Google API key related-code queries");
        }
    }

    let search = PanicSearch;
    let focal_code = "AIzaSyDabcdefghijklmnopqrstuvwxYZABCDEFGHIJKLMNOPQ";
    let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
    assert!(
        req.related_code.is_empty(),
        "expected no related code for Google API key-only focal code"
    );
}

#[test]
fn related_code_query_skips_aws_access_key_id_only_selections() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for AWS access key ID related-code queries");
        }
    }

    let search = PanicSearch;
    let focal_codes = [
        ["AK", "IA", "IOSFODNN7EXAMPLE"].concat(),
        ["AS", "IA", "IOSFODNN7EXAMPLE"].concat(),
    ];
    for focal_code in &focal_codes {
        let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
        assert!(
            req.related_code.is_empty(),
            "expected no related code for AWS access key ID-only focal code"
        );
    }
}

#[test]
fn related_code_query_skips_google_oauth_client_secret_only_selections() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for Google client secret related-code queries");
        }
    }

    let search = PanicSearch;
    let focal_code = ["GOC", "SPX", "-", "not-a-real-client-secret-but-long-enough"].concat();
    let req = base_request(&focal_code).with_related_code_from_focal(&search, 3);
    assert!(
        req.related_code.is_empty(),
        "expected no related code for Google client secret-only focal code"
    );
}

#[test]
fn related_code_query_skips_github_pat_only_selections() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for GitHub PAT related-code queries");
        }
    }

    let search = PanicSearch;
    let focal_code = "github_pat_ABCDEFGHIJKLMNOPQRST";
    let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
    assert!(
        req.related_code.is_empty(),
        "expected no related code for GitHub PAT-only focal code"
    );
}

#[test]
fn related_code_query_skips_gitlab_pat_only_selections() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for GitLab PAT related-code queries");
        }
    }

    let search = PanicSearch;
    let focal_code = "glpat-ABCDEFGHIJKLMNOPQRST";
    let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
    assert!(
        req.related_code.is_empty(),
        "expected no related code for GitLab PAT-only focal code"
    );
}

#[test]
fn related_code_query_skips_stripe_secret_key_only_selections() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for Stripe secret key related-code queries");
        }
    }

    let search = PanicSearch;
    let focal_codes = [
        ["sk", "_", "live", "_", "not-a-real-stripe-key-but-long-enough"].concat(),
        ["rk", "_", "test", "_", "not-a-real-stripe-key-but-long-enough"].concat(),
        ["wh", "sec", "_", "not-a-real-webhook-secret-but-long-enough"].concat(),
    ];
    for focal_code in &focal_codes {
        let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
        assert!(
            req.related_code.is_empty(),
            "expected no related code for Stripe secret key-only focal code"
        );
    }
}

#[test]
fn related_code_query_skips_slack_token_only_selections() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for Slack token related-code queries");
        }
    }

    let search = PanicSearch;
    // These tokens are constructed from parts so we can exercise the secret-detection heuristics
    // without committing a literal that trips GitHub push-protection scanners.
    let focal_codes = [
        ["xox", "b", "-", "not", "-", "a", "-", "real", "-", "token", "-but-long-enough"].concat(),
        ["xap", "p", "-", "1", "-", "not", "-", "a", "-", "real", "-", "token", "-but-long-enough"]
            .concat(),
    ];
    for focal_code in &focal_codes {
        let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
        assert!(
            req.related_code.is_empty(),
            "expected no related code for Slack token-only focal code"
        );
    }
}

#[test]
fn related_code_query_skips_google_oauth_token_only_selections() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for OAuth token related-code queries");
        }
    }

    let search = PanicSearch;
    let focal_code = ["ya29", ".", "a0ARrdaM", "-", "not-a-real-token-but-long-enough"].concat();
    let req = base_request(&focal_code).with_related_code_from_focal(&search, 3);
    assert!(
        req.related_code.is_empty(),
        "expected no related code for OAuth token-only focal code"
    );
}

#[test]
fn related_code_query_skips_high_entropy_token_only_selections() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for high-entropy related-code queries");
        }
    }

    let search = PanicSearch;
    let focal_code = "AbCDefGhijkLMNOPqrstuVWXYZ0123456789abcdef";
    let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
    assert!(
        req.related_code.is_empty(),
        "expected no related code for high-entropy focal code"
    );
}

#[test]
fn related_code_query_skips_hex_hash_only_selections() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for hex-hash-only related-code queries");
        }
    }

    let search = PanicSearch;
    let focal_code = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
    let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
    assert!(
        req.related_code.is_empty(),
        "expected no related code for hex-hash-only focal code"
    );
}

#[test]
fn related_code_query_skips_numeric_literal_only_selections() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for numeric-literal-only selections");
        }
    }

    let search = PanicSearch;
    let focal_code = "0xDEADBEEF";
    let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
    assert!(
        req.related_code.is_empty(),
        "expected no related code for numeric-literal-only focal code"
    );
}

#[test]
fn related_code_query_ignores_numeric_literal_fragments() {
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
    let focal_code = "double x = 0x1.ffffp10; return foo.bar();";

    let _ = base_request(focal_code).with_related_code_from_focal(&search, 1);
    let query = search
        .last_query
        .lock()
        .expect("lock poisoned")
        .clone()
        .expect("query captured");

    let lower = query.to_ascii_lowercase();
    assert!(
        query.contains("foo") || query.contains("bar"),
        "expected query to retain code identifiers, got: {query}"
    );
    assert!(
        !lower.contains("ffff") && !lower.contains("p10") && !lower.contains("deadbeef"),
        "expected query to ignore numeric literal fragments, got: {query}"
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
