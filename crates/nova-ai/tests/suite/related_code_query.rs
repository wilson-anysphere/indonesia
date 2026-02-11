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

    let private_segment = "NOVA_AI_PRIVATE_USER_12345";
    for sep in [
        // Single-encoded.
        "%2F",
        "%5C",
        // Double-encoded.
        "%252F",
        "%255C",
        // Triple-encoded.
        "%25252F",
        "%25255C",
        // Quad-encoded.
        "%2525252F",
        "%2525255C",
    ] {
        let search = CapturingSearch::default();
        let focal_code = format!(
            "{sep}home{sep}user{sep}my-{private_segment}-project{sep}src{sep}main{sep}java\nreturn foo.bar();\n"
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
}

#[test]
fn related_code_query_avoids_html_entity_percent_encoded_path_segments() {
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

    let private_segment = "NOVA_AI_PRIVATE_USER_12345";
    for sep in [
        // `%2F` via HTML entity percent sign.
        "&#37;2F",
        "&#x25;2F",
        "&percnt;2F",
        // Nested HTML escaping of the percent entity itself.
        "&amp;#37;2F",
        "&amp;percnt;2F",
        // Nested percent-encoding (`%252F`) with HTML entity percent sign.
        "&#37;252F",
        "&amp;#37;252F",
        // Backslash separator (`%5C`).
        "&#37;5C",
        "&percnt;5C",
        "&amp;#37;5C",
        // Nested percent-encoded backslash (`%255C`).
        "&#37;255C",
    ] {
        let search = CapturingSearch::default();
        let focal_code = format!(
            "{sep}home{sep}user{sep}my-{private_segment}-project{sep}src{sep}main{sep}java\nreturn foo.bar();\n"
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
            "query should not include percent-encoded path fragments hidden behind HTML entity percent signs: {query}"
        );
        assert!(
            query.contains("foo") || query.contains("bar"),
            "expected query to retain non-path identifiers, got: {query}"
        );
    }
}

#[test]
fn related_code_query_avoids_html_entity_percent_encoded_path_segments_without_semicolons() {
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

    let private_segment = "NOVA_AI_PRIVATE_USER_12345";
    for sep in [
        // `%2F` via HTML entity percent sign, without `;` terminator.
        "&#372F",
        "&#x252F",
        "&percnt2F",
        // Nested HTML escaping of the percent entity itself.
        "&amp;#372F",
        "&amp;percnt2F",
        // Backslash separator (`%5C`).
        "&#375C",
        "&#x255C",
        "&percnt5C",
        "&amp;#375C",
    ] {
        let search = CapturingSearch::default();
        let focal_code = format!(
            "{sep}home{sep}user{sep}my-{private_segment}-project{sep}src{sep}main{sep}java\nreturn foo.bar();\n"
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
            "query should not include percent-encoded path fragments hidden behind HTML entity percent signs without semicolons: {query}"
        );
        assert!(
            query.contains("foo") || query.contains("bar"),
            "expected query to retain non-path identifiers, got: {query}"
        );
    }
}

#[test]
fn related_code_query_avoids_unicode_separator_path_segments() {
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

    let private_segment = "NOVA_AI_PRIVATE_USER_12345";
    for sep in [
        "\u{2215}", // ∕ division slash
        "\u{2044}", // ⁄ fraction slash
        "\u{FF0F}", // ／ fullwidth solidus
        "\u{29F8}", // ⧸ big solidus
        "\u{2216}", // ∖ set minus / backslash-like
        "\u{FF3C}", // ＼ fullwidth reverse solidus
        "\u{29F9}", // ⧹ big reverse solidus
        "\u{FE68}", // ﹨ small reverse solidus
    ] {
        let search = CapturingSearch::default();
        let focal_code = format!(
            "{sep}home{sep}user{sep}my-{private_segment}-project{sep}src{sep}main{sep}java\nreturn foo.bar();\n"
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
            "query should not include unicode path fragments: {query}"
        );
        assert!(
            query.contains("foo") || query.contains("bar"),
            "expected query to retain non-path identifiers, got: {query}"
        );
    }
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

    for prefix in [
        r"\u002Fhome\u002Fuser\u002Fmy-",
        r"\u{002F}home\u{002F}user\u{002F}my-",
    ] {
        let search = CapturingSearch::default();
        let private_segment = "NOVA_AI_PRIVATE_USER_12345";
        let focal_code = [
            prefix,
            private_segment,
            r"-project\u002Fsrc\u002Fmain\u002Fjava",
            "\nreturn foo.bar();\n",
        ]
        .concat();

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
}

#[test]
fn related_code_query_avoids_hex_escaped_path_segments() {
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

    let private_segment = "NOVA_AI_PRIVATE_USER_12345";
    for prefix in [r"\x2Fhome\x2Fuser\x2Fmy-", r"\x{2F}home\x{2F}user\x{2F}my-"] {
        let search = CapturingSearch::default();
        let focal_code = [
            prefix,
            private_segment,
            r"-project\x2Fsrc\x2Fmain\x2Fjava",
            "\nreturn foo.bar();\n",
        ]
        .concat();

        let _ = base_request(&focal_code).with_related_code_from_focal(&search, 1);
        let query = search
            .last_query
            .lock()
            .expect("lock poisoned")
            .clone()
            .expect("query captured");

        assert!(
            !query.contains(private_segment),
            "query should not include hex-escaped path fragments: {query}"
        );
        assert!(
            query.contains("foo") || query.contains("bar"),
            "expected query to retain non-path identifiers, got: {query}"
        );
    }
}

#[test]
fn related_code_query_avoids_html_entity_path_segments() {
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

    let private_segment = "NOVA_AI_PRIVATE_USER_12345";
    for sep in ["&#47;", "&#x2F;", "&#92;", "&#x5C;"] {
        let search = CapturingSearch::default();
        let focal_code = format!(
            "{sep}home{sep}user{sep}my-{private_segment}-project{sep}src{sep}main{sep}java\nreturn foo.bar();\n"
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
            "query should not include HTML entity path fragments: {query}"
        );
        assert!(
            query.contains("foo") || query.contains("bar"),
            "expected query to retain non-path identifiers, got: {query}"
        );
    }
}

#[test]
fn related_code_query_avoids_html_entity_path_segments_without_semicolons() {
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

    let private_segment = "NOVA_AI_PRIVATE_USER_12345";
    for sep in ["&#47", "&#x2F", "&#92", "&#x5C"] {
        let search = CapturingSearch::default();
        let focal_code = format!(
            "{sep}home{sep}user{sep}my-{private_segment}-project{sep}src{sep}main{sep}java\nreturn foo.bar();\n"
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
            "query should not include HTML entity path fragments without semicolons: {query}"
        );
        assert!(
            query.contains("foo") || query.contains("bar"),
            "expected query to retain non-path identifiers, got: {query}"
        );
    }
}

#[test]
fn related_code_query_avoids_html_entity_unicode_separator_path_segments() {
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

    let private_segment = "NOVA_AI_PRIVATE_USER_12345";
    for sep in [
        // Slash-like separators.
        "&#8725;",  // ∕ division slash (U+2215)
        "&#8260;",  // ⁄ fraction slash (U+2044)
        "&#65295;", // ／ fullwidth solidus (U+FF0F)
        "&#10744;", // ⧸ big solidus (U+29F8)
        "&frasl;",  // ⁄ fraction slash (named entity)
        // Backslash-like separators.
        "&#8726;",  // ∖ set minus / backslash-like (U+2216)
        "&#65340;", // ＼ fullwidth reverse solidus (U+FF3C)
        "&#10745;", // ⧹ big reverse solidus (U+29F9)
        "&#65128;", // ﹨ small reverse solidus (U+FE68)
        "&setminus;", // ∖ set minus (named entity)
    ] {
        let search = CapturingSearch::default();
        let focal_code = format!(
            "{sep}home{sep}user{sep}my-{private_segment}-project{sep}src{sep}main{sep}java\nreturn foo.bar();\n"
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
            "query should not include HTML entity unicode path fragments: {query}"
        );
        assert!(
            query.contains("foo") || query.contains("bar"),
            "expected query to retain non-path identifiers, got: {query}"
        );
    }
}

#[test]
fn related_code_query_avoids_html_entity_unicode_separator_path_segments_without_semicolons() {
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

    let private_segment = "NOVA_AI_PRIVATE_USER_12345";
    for sep in [
        // Slash-like separators.
        "&#8725",  // ∕ division slash (U+2215)
        "&#8260",  // ⁄ fraction slash (U+2044)
        "&#65295", // ／ fullwidth solidus (U+FF0F)
        "&#10744", // ⧸ big solidus (U+29F8)
        "&frasl",
        "&amp;frasl",
        "&amp;amp;frasl",
        // Backslash-like separators.
        "&#8726",  // ∖ set minus (U+2216)
        "&#65340", // ＼ fullwidth reverse solidus (U+FF3C)
        "&#10745", // ⧹ big reverse solidus (U+29F9)
        "&#65128", // ﹨ small reverse solidus (U+FE68)
        "&setminus",
        "&amp;setminus",
        "&amp;amp;setminus",
    ] {
        let search = CapturingSearch::default();
        let focal_code = format!(
            "{sep}home{sep}user{sep}my-{private_segment}-project{sep}src{sep}main{sep}java\nreturn foo.bar();\n"
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
            "query should not include HTML entity unicode path fragments without semicolons: {query}"
        );
        assert!(
            query.contains("foo") || query.contains("bar"),
            "expected query to retain non-path identifiers, got: {query}"
        );
    }
}

#[test]
fn related_code_query_avoids_double_escaped_html_entity_path_segments_without_semicolons() {
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

    let private_segment = "NOVA_AI_PRIVATE_USER_12345";
    for sep in [
        "&amp;#47",
        "&amp;#x2F",
        "&amp;#92",
        "&amp;#x5C",
        "&amp;amp;#47",
        "&amp;amp;#x2F",
        "&amp;amp;#92",
        "&amp;amp;#x5C",
    ] {
        let search = CapturingSearch::default();
        let focal_code = format!(
            "{sep}home{sep}user{sep}my-{private_segment}-project{sep}src{sep}main{sep}java\nreturn foo.bar();\n"
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
            "query should not include double-escaped HTML entity fragments without semicolons: {query}"
        );
        assert!(
            query.contains("foo") || query.contains("bar"),
            "expected query to retain non-path identifiers, got: {query}"
        );
    }
}

#[test]
fn related_code_query_avoids_named_html_entity_path_segments() {
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

    let private_segment = "NOVA_AI_PRIVATE_USER_12345";
    for sep in ["&sol;", "&bsol;", "&Backslash;"] {
        let search = CapturingSearch::default();
        let focal_code = format!(
            "{sep}home{sep}user{sep}my-{private_segment}-project{sep}src{sep}main{sep}java\nreturn foo.bar();\n"
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
            "query should not include named HTML entity path fragments: {query}"
        );
        assert!(
            query.contains("foo") || query.contains("bar"),
            "expected query to retain non-path identifiers, got: {query}"
        );
    }
}

#[test]
fn related_code_query_avoids_named_html_entity_path_segments_without_semicolons() {
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

    let private_segment = "NOVA_AI_PRIVATE_USER_12345";
    for sep in [
        "&sol",
        "&bsol",
        "&Backslash",
        "&amp;sol",
        "&amp;bsol",
        "&amp;Backslash",
        "&amp;amp;sol",
        "&amp;amp;bsol",
        "&amp;amp;Backslash",
    ] {
        let search = CapturingSearch::default();
        let focal_code = format!(
            "{sep}home{sep}user{sep}my-{private_segment}-project{sep}src{sep}main{sep}java\nreturn foo.bar();\n"
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
            "query should not include named HTML entity path fragments without semicolons: {query}"
        );
        assert!(
            query.contains("foo") || query.contains("bar"),
            "expected query to retain non-path identifiers, got: {query}"
        );
    }
}

#[test]
fn related_code_query_avoids_double_escaped_html_entity_path_segments() {
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

    let private_segment = "NOVA_AI_PRIVATE_USER_12345";
    for sep in [
        "&amp;#47;",
        "&amp;#x2F;",
        "&amp;#92;",
        "&amp;#x5C;",
        "&amp;amp;#47;",
        "&amp;amp;#x2F;",
        "&amp;amp;#92;",
        "&amp;amp;#x5C;",
        "&amp;sol;",
        "&amp;bsol;",
        "&amp;Backslash;",
        "&amp;amp;sol;",
        "&amp;amp;bsol;",
        "&amp;amp;Backslash;",
    ] {
        let search = CapturingSearch::default();
        let focal_code = format!(
            "{sep}home{sep}user{sep}my-{private_segment}-project{sep}src{sep}main{sep}java\nreturn foo.bar();\n"
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
            "query should not include double-escaped HTML entity path fragments: {query}"
        );
        assert!(
            query.contains("foo") || query.contains("bar"),
            "expected query to retain non-path identifiers, got: {query}"
        );
    }
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
    for focal_code in [
        r#""service.internal""#,
        r#""example.com""#,
        "service.internal",
        "example.com",
    ] {
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
        "%25252Fhome%25252Fuser%25252Fsecret%25252Fcredentials",
        "%25255Chome%25255Cuser%25255Csecret%25255Ccredentials",
        "%2525252Fhome%2525252Fuser%2525252Fsecret%2525252Fcredentials",
        "%2525255Chome%2525255Cuser%2525255Csecret%2525255Ccredentials",
    ] {
        let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
        assert!(
            req.related_code.is_empty(),
            "expected no related code for percent-encoded path-only focal code"
        );
    }
}

#[test]
fn related_code_query_skips_html_entity_percent_encoded_path_only_selections() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for HTML entity percent-encoded path selections");
        }
    }

    let search = PanicSearch;
    for focal_code in [
        "&#37;2Fhome&#37;2Fuser&#37;2Fsecret&#37;2Fcredentials",
        "&#x25;2Fhome&#x25;2Fuser&#x25;2Fsecret&#x25;2Fcredentials",
        "&percnt;2Fhome&percnt;2Fuser&percnt;2Fsecret&percnt;2Fcredentials",
        "&amp;#37;2Fhome&amp;#37;2Fuser&amp;#37;2Fsecret&amp;#37;2Fcredentials",
        "&amp;percnt;2Fhome&amp;percnt;2Fuser&amp;percnt;2Fsecret&amp;percnt;2Fcredentials",
        "&#37;252Fhome&#37;252Fuser&#37;252Fsecret&#37;252Fcredentials",
        "&amp;#37;252Fhome&amp;#37;252Fuser&amp;#37;252Fsecret&amp;#37;252Fcredentials",
        "&#37;5Chome&#37;5Cuser&#37;5Csecret&#37;5Ccredentials",
        "&percnt;5Chome&percnt;5Cuser&percnt;5Csecret&percnt;5Ccredentials",
        "&amp;#37;5Chome&amp;#37;5Cuser&amp;#37;5Csecret&amp;#37;5Ccredentials",
    ] {
        let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
        assert!(
            req.related_code.is_empty(),
            "expected no related code for HTML entity percent-encoded path-only focal code"
        );
    }
}

#[test]
fn related_code_query_skips_html_entity_percent_encoded_path_only_selections_without_semicolons() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for HTML entity percent-encoded path selections without semicolons");
        }
    }

    let search = PanicSearch;
    for focal_code in [
        "&#372Fhome&#372Fuser&#372Fsecret&#372Fcredentials",
        "&#x252Fhome&#x252Fuser&#x252Fsecret&#x252Fcredentials",
        "&percnt2Fhome&percnt2Fuser&percnt2Fsecret&percnt2Fcredentials",
        "&amp;#372Fhome&amp;#372Fuser&amp;#372Fsecret&amp;#372Fcredentials",
        "&amp;percnt2Fhome&amp;percnt2Fuser&amp;percnt2Fsecret&amp;percnt2Fcredentials",
        "&#375Chome&#375Cuser&#375Csecret&#375Ccredentials",
        "&#x255Chome&#x255Cuser&#x255Csecret&#x255Ccredentials",
        "&percnt5Chome&percnt5Cuser&percnt5Csecret&percnt5Ccredentials",
        "&amp;#375Chome&amp;#375Cuser&amp;#375Csecret&amp;#375Ccredentials",
    ] {
        let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
        assert!(
            req.related_code.is_empty(),
            "expected no related code for HTML entity percent-encoded path-only focal code without semicolons"
        );
    }
}

#[test]
fn related_code_query_skips_unicode_separator_path_only_selections() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for unicode path separator selections");
        }
    }

    let search = PanicSearch;
    for sep in [
        "\u{2215}", // ∕ division slash
        "\u{2044}", // ⁄ fraction slash
        "\u{FF0F}", // ／ fullwidth solidus
        "\u{29F8}", // ⧸ big solidus
        "\u{2216}", // ∖ set minus / backslash-like
        "\u{FF3C}", // ＼ fullwidth reverse solidus
        "\u{29F9}", // ⧹ big reverse solidus
        "\u{FE68}", // ﹨ small reverse solidus
    ] {
        let focal_code = format!("{sep}home{sep}user{sep}secret{sep}credentials");
        let req = base_request(&focal_code).with_related_code_from_focal(&search, 3);
        assert!(
            req.related_code.is_empty(),
            "expected no related code for unicode separator path-only focal code"
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
        r"\u{002F}home\u{002F}user\u{002F}secret\u{002F}credentials",
        r"\u{005C}home\u{005C}user\u{005C}secret\u{005C}credentials",
        r"\U0000002Fhome\U0000002Fuser\U0000002Fsecret\U0000002Fcredentials",
        r"\U0000005Chome\U0000005Cuser\U0000005Csecret\U0000005Ccredentials",
    ] {
        let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
        assert!(
            req.related_code.is_empty(),
            "expected no related code for unicode-escaped path-only focal code"
        );
    }
}

#[test]
fn related_code_query_skips_hex_escaped_path_only_selections() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for hex-escaped path selections");
        }
    }

    let search = PanicSearch;
    for focal_code in [
        r"\x2Fhome\x2Fuser\x2Fsecret\x2Fcredentials",
        r"\x5Chome\x5Cuser\x5Csecret\x5Ccredentials",
        r"\x{2F}home\x{2F}user\x{2F}secret\x{2F}credentials",
        r"\x{5C}home\x{5C}user\x{5C}secret\x{5C}credentials",
    ] {
        let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
        assert!(
            req.related_code.is_empty(),
            "expected no related code for hex-escaped path-only focal code"
        );
    }
}

#[test]
fn related_code_query_skips_html_entity_path_only_selections() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for HTML entity path selections");
        }
    }

    let search = PanicSearch;
    for focal_code in [
        "&#47;home&#47;user&#47;secret&#47;credentials",
        "&#x2F;home&#x2F;user&#x2F;secret&#x2F;credentials",
        "&#92;home&#92;user&#92;secret&#92;credentials",
        "&#x5C;home&#x5C;user&#x5C;secret&#x5C;credentials",
    ] {
        let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
        assert!(
            req.related_code.is_empty(),
            "expected no related code for HTML entity path-only focal code"
        );
    }
}

#[test]
fn related_code_query_skips_html_entity_path_only_selections_without_semicolons() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for HTML entity path selections without semicolons");
        }
    }

    let search = PanicSearch;
    for focal_code in [
        "&#47home&#47user&#47secret&#47credentials",
        "&#x2Fhome&#x2Fuser&#x2Fsecret&#x2Fcredentials",
        "&#92home&#92user&#92secret&#92credentials",
        "&#x5Chome&#x5Cuser&#x5Csecret&#x5Ccredentials",
    ] {
        let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
        assert!(
            req.related_code.is_empty(),
            "expected no related code for HTML entity path-only focal code without semicolons"
        );
    }
}

#[test]
fn related_code_query_skips_html_entity_unicode_separator_path_only_selections() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for HTML entity unicode path selections");
        }
    }

    let search = PanicSearch;
    for focal_code in [
        "&#8725;home&#8725;user&#8725;secret&#8725;credentials",
        "&#8260;home&#8260;user&#8260;secret&#8260;credentials",
        "&#65295;home&#65295;user&#65295;secret&#65295;credentials",
        "&#10744;home&#10744;user&#10744;secret&#10744;credentials",
        "&frasl;home&frasl;user&frasl;secret&frasl;credentials",
        "&#8726;home&#8726;user&#8726;secret&#8726;credentials",
        "&#65340;home&#65340;user&#65340;secret&#65340;credentials",
        "&#10745;home&#10745;user&#10745;secret&#10745;credentials",
        "&#65128;home&#65128;user&#65128;secret&#65128;credentials",
        "&setminus;home&setminus;user&setminus;secret&setminus;credentials",
    ] {
        let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
        assert!(
            req.related_code.is_empty(),
            "expected no related code for HTML entity unicode path-only focal code"
        );
    }
}

#[test]
fn related_code_query_skips_html_entity_unicode_separator_path_only_selections_without_semicolons() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for HTML entity unicode path selections without semicolons");
        }
    }

    let search = PanicSearch;
    for focal_code in [
        "&#8725home&#8725user&#8725secret&#8725credentials",
        "&#8260home&#8260user&#8260secret&#8260credentials",
        "&#65295home&#65295user&#65295secret&#65295credentials",
        "&#10744home&#10744user&#10744secret&#10744credentials",
        "&fraslhome&frasluser&fraslsecret&fraslcredentials",
        "&amp;fraslhome&amp;frasluser&amp;fraslsecret&amp;fraslcredentials",
        "&amp;amp;fraslhome&amp;amp;frasluser&amp;amp;fraslsecret&amp;amp;fraslcredentials",
        "&#8726home&#8726user&#8726secret&#8726credentials",
        "&#65340home&#65340user&#65340secret&#65340credentials",
        "&#10745home&#10745user&#10745secret&#10745credentials",
        "&#65128home&#65128user&#65128secret&#65128credentials",
        "&setminushome&setminususer&setminussecret&setminuscredentials",
        "&amp;setminushome&amp;setminususer&amp;setminussecret&amp;setminuscredentials",
        "&amp;amp;setminushome&amp;amp;setminususer&amp;amp;setminussecret&amp;amp;setminuscredentials",
    ] {
        let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
        assert!(
            req.related_code.is_empty(),
            "expected no related code for HTML entity unicode path-only focal code without semicolons"
        );
    }
}

#[test]
fn related_code_query_skips_double_escaped_html_entity_path_only_selections_without_semicolons() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for double-escaped HTML entity path selections without semicolons");
        }
    }

    let search = PanicSearch;
    for focal_code in [
        "&amp;#47home&amp;#47user&amp;#47secret&amp;#47credentials",
        "&amp;#x2Fhome&amp;#x2Fuser&amp;#x2Fsecret&amp;#x2Fcredentials",
        "&amp;#92home&amp;#92user&amp;#92secret&amp;#92credentials",
        "&amp;#x5Chome&amp;#x5Cuser&amp;#x5Csecret&amp;#x5Ccredentials",
        "&amp;amp;#47home&amp;amp;#47user&amp;amp;#47secret&amp;amp;#47credentials",
        "&amp;amp;#x2Fhome&amp;amp;#x2Fuser&amp;amp;#x2Fsecret&amp;amp;#x2Fcredentials",
        "&amp;amp;#92home&amp;amp;#92user&amp;amp;#92secret&amp;amp;#92credentials",
        "&amp;amp;#x5Chome&amp;amp;#x5Cuser&amp;amp;#x5Csecret&amp;amp;#x5Ccredentials",
    ] {
        let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
        assert!(
            req.related_code.is_empty(),
            "expected no related code for double-escaped HTML entity path-only focal code without semicolons"
        );
    }
}

#[test]
fn related_code_query_skips_named_html_entity_path_only_selections() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for named HTML entity path selections");
        }
    }

    let search = PanicSearch;
    for focal_code in [
        "&sol;home&sol;user&sol;secret&sol;credentials",
        "&bsol;home&bsol;user&bsol;secret&bsol;credentials",
        "&Backslash;home&Backslash;user&Backslash;secret&Backslash;credentials",
    ] {
        let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
        assert!(
            req.related_code.is_empty(),
            "expected no related code for named HTML entity path-only focal code"
        );
    }
}

#[test]
fn related_code_query_skips_named_html_entity_path_only_selections_without_semicolons() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for named HTML entity path selections without semicolons");
        }
    }

    let search = PanicSearch;
    for focal_code in [
        "&solhome&soluser&solsecret&solcredentials",
        "&bsolhome&bsoluser&bsolsecret&bsolcredentials",
        "&Backslashhome&Backslashuser&Backslashsecret&Backslashcredentials",
        "&amp;solhome&amp;soluser&amp;solsecret&amp;solcredentials",
        "&amp;bsolhome&amp;bsoluser&amp;bsolsecret&amp;bsolcredentials",
        "&amp;Backslashhome&amp;Backslashuser&amp;Backslashsecret&amp;Backslashcredentials",
        "&amp;amp;solhome&amp;amp;soluser&amp;amp;solsecret&amp;amp;solcredentials",
        "&amp;amp;bsolhome&amp;amp;bsoluser&amp;amp;bsolsecret&amp;amp;bsolcredentials",
        "&amp;amp;Backslashhome&amp;amp;Backslashuser&amp;amp;Backslashsecret&amp;amp;Backslashcredentials",
    ] {
        let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
        assert!(
            req.related_code.is_empty(),
            "expected no related code for named HTML entity path-only focal code without semicolons"
        );
    }
}

#[test]
fn related_code_query_skips_double_escaped_html_entity_path_only_selections() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for double-escaped HTML entity path selections");
        }
    }

    let search = PanicSearch;
    for focal_code in [
        "&amp;#47;home&amp;#47;user&amp;#47;secret&amp;#47;credentials",
        "&amp;#x2F;home&amp;#x2F;user&amp;#x2F;secret&amp;#x2F;credentials",
        "&amp;#92;home&amp;#92;user&amp;#92;secret&amp;#92;credentials",
        "&amp;#x5C;home&amp;#x5C;user&amp;#x5C;secret&amp;#x5C;credentials",
        "&amp;amp;#47;home&amp;amp;#47;user&amp;amp;#47;secret&amp;amp;#47;credentials",
        "&amp;amp;#x2F;home&amp;amp;#x2F;user&amp;amp;#x2F;secret&amp;amp;#x2F;credentials",
        "&amp;amp;#92;home&amp;amp;#92;user&amp;amp;#92;secret&amp;amp;#92;credentials",
        "&amp;amp;#x5C;home&amp;amp;#x5C;user&amp;amp;#x5C;secret&amp;amp;#x5C;credentials",
        "&amp;sol;home&amp;sol;user&amp;sol;secret&amp;sol;credentials",
        "&amp;bsol;home&amp;bsol;user&amp;bsol;secret&amp;bsol;credentials",
        "&amp;Backslash;home&amp;Backslash;user&amp;Backslash;secret&amp;Backslash;credentials",
        "&amp;amp;sol;home&amp;amp;sol;user&amp;amp;sol;secret&amp;amp;sol;credentials",
        "&amp;amp;bsol;home&amp;amp;bsol;user&amp;amp;bsol;secret&amp;amp;bsol;credentials",
        "&amp;amp;Backslash;home&amp;amp;Backslash;user&amp;amp;Backslash;secret&amp;amp;Backslash;credentials",
    ] {
        let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
        assert!(
            req.related_code.is_empty(),
            "expected no related code for double-escaped HTML entity path-only focal code"
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
fn related_code_query_skips_base64url_triplet_only_selections() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for base64url-triplet related-code queries");
        }
    }

    let search = PanicSearch;
    // Exercise the heuristic without committing a literal that might look like a real token.
    let focal_code = [
        "AbcdefGhijklmnopqrstUVWX",
        ".",
        "abcdef",
        ".",
        "Zyxwvutsrqponmlkjihg_fedcba-XYZ",
    ]
    .concat();
    let req = base_request(&focal_code).with_related_code_from_focal(&search, 3);
    assert!(
        req.related_code.is_empty(),
        "expected no related code for base64url-triplet-only focal code"
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
fn related_code_query_skips_base32_only_selections() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for base32-only related-code queries");
        }
    }

    let search = PanicSearch;
    let focal_code = "JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP";
    let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
    assert!(
        req.related_code.is_empty(),
        "expected no related code for base32-only focal code"
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
fn related_code_query_skips_common_api_token_prefixes_only_selections() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for token prefix related-code queries");
        }
    }

    let search = PanicSearch;
    let focal_codes = [
        ["S", "G", ".", "not-a-real-sendgrid-key-but-long-enough"].concat(),
        ["h", "f", "_", "not-a-real-hf-token-but-long-enough"].concat(),
        ["do", "p", "_v1", "_", "not-a-real-do-token-but-long-enough"].concat(),
    ];
    for focal_code in &focal_codes {
        let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
        assert!(
            req.related_code.is_empty(),
            "expected no related code for token-only focal code"
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
fn related_code_query_skips_discord_and_square_token_prefixes_only_selections() {
    struct PanicSearch;

    impl SemanticSearch for PanicSearch {
        fn search(&self, _query: &str) -> Vec<SearchResult> {
            panic!("search should not be called for token prefix related-code queries");
        }
    }

    let search = PanicSearch;
    let focal_codes = [
        ["m", "fa", ".", "not-a-real-discord-token-but-long-enough"].concat(),
        ["sq0", "atp", "-", "not-a-real-square-token-but-long-enough"].concat(),
        ["sq0", "csp", "-", "not-a-real-square-token-but-long-enough"].concat(),
    ];
    for focal_code in &focal_codes {
        let req = base_request(focal_code).with_related_code_from_focal(&search, 3);
        assert!(
            req.related_code.is_empty(),
            "expected no related code for token-only focal code"
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
