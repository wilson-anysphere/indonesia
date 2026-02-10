use std::path::PathBuf;

use nova_ai::{SemanticSearch, TrigramSemanticSearch, VirtualWorkspace};

fn assert_result_range(original: &str, query: &str, result: &nova_ai::SearchResult) {
    assert!(
        result.range.start < result.range.end,
        "expected non-empty range, got {:?}",
        result.range
    );
    assert!(
        result.range.end <= original.len(),
        "range {:?} out of bounds for len={}",
        result.range,
        original.len()
    );
    assert!(
        original.is_char_boundary(result.range.start),
        "range start {:?} is not a char boundary",
        result.range
    );
    assert!(
        original.is_char_boundary(result.range.end),
        "range end {:?} is not a char boundary",
        result.range
    );

    let slice = original
        .get(result.range.clone())
        .expect("range should be valid UTF-8 slice");

    assert_eq!(
        result.snippet,
        slice.trim(),
        "snippet should align with range"
    );

    let query_lower = query.to_lowercase();
    let original_lower = original.to_lowercase();
    if original_lower.contains(&query_lower) {
        assert!(
            slice.to_lowercase().contains(&query_lower),
            "expected original[range] to contain query (case-insensitive)\nquery={query:?}\nrange={:?}\nslice={slice:?}",
            result.range
        );
    }
}

#[test]
fn trigram_index_file_then_search_returns_results() {
    let mut search = TrigramSemanticSearch::new();
    let original = "public class Hello { String hello() { return \"hello\"; } }".to_string();
    search.index_file(PathBuf::from("src/Hello.java"), original.clone());

    let results = search.search("hello");
    assert!(!results.is_empty());
    assert_eq!(results[0].path, PathBuf::from("src/Hello.java"));
    assert!(results[0].snippet.to_lowercase().contains("hello"));
    assert_result_range(&original, "hello", &results[0]);
}

#[test]
fn trigram_upsert_updates_ranking_and_snippet() {
    let mut search = TrigramSemanticSearch::new();
    let path_a = PathBuf::from("src/a.txt");
    let path_b = PathBuf::from("src/b.txt");
    let text_a = "hello abcdefghijklmnopqrstuvwxyz".to_string();
    let text_b_initial = "helicopter landing pad".to_string();

    // Path A is initially the strongest match because it contains "hello" as a substring.
    search.index_file(path_a.clone(), text_a.clone());
    search.index_file(path_b.clone(), text_b_initial.clone());

    let before = search.search("hello");
    assert_eq!(before[0].path, path_a);

    let before_a = before
        .iter()
        .find(|r| r.path == path_a)
        .expect("expected a.txt to be in results");
    assert_result_range(&text_a, "hello", before_a);

    let before_b_result = before
        .iter()
        .find(|r| r.path == path_b)
        .expect("expected b.txt to have a non-zero trigram score")
        .clone();
    assert_result_range(&text_b_initial, "hello", &before_b_result);
    let before_b = before_b_result.snippet;

    // Upsert B with content that matches the query exactly; B should now outrank A.
    let text_b_updated = "hello".to_string();
    search.index_file(path_b.clone(), text_b_updated.clone());

    let after = search.search("hello");
    assert_eq!(after[0].path, path_b);
    assert_ne!(after[0].snippet, before_b);
    assert!(after[0].snippet.to_lowercase().contains("hello"));
    assert_result_range(&text_b_updated, "hello", &after[0]);
}

#[test]
fn trigram_remove_file_removes_from_results() {
    let mut search = TrigramSemanticSearch::new();
    let path_a = PathBuf::from("src/a.txt");
    let path_b = PathBuf::from("src/b.txt");
    let text_a = "hello world".to_string();
    let text_b = "hello there".to_string();

    search.index_file(path_a.clone(), text_a.clone());
    search.index_file(path_b.clone(), text_b.clone());
    assert!(search.search("hello").iter().any(|r| r.path == path_a));

    search.remove_file(path_a.as_path());
    let results = search.search("hello");
    assert!(!results.iter().any(|r| r.path == path_a));
    let b = results
        .iter()
        .find(|r| r.path == path_b)
        .expect("expected b.txt to remain");
    assert_result_range(&text_b, "hello", b);
}

#[test]
fn trigram_index_project_matches_repeated_index_file() {
    let files = vec![
        ("src/a.txt", "hello world"),
        ("src/b.txt", "helicopter landing pad"),
    ];

    let db = VirtualWorkspace::new(
        files
            .iter()
            .map(|(path, text)| (path.to_string(), text.to_string())),
    );

    let mut by_project = TrigramSemanticSearch::new();
    by_project.index_project(&db);

    let mut by_file = TrigramSemanticSearch::new();
    for (path, text) in files {
        by_file.index_file(PathBuf::from(path), text.to_string());
    }

    assert_eq!(by_project.search("hello"), by_file.search("hello"));
}

#[test]
fn trigram_fuzzy_matches_return_prefix_window_range() {
    let mut search = TrigramSemanticSearch::new();

    let path = PathBuf::from("src/long.txt");
    let mut original = String::new();
    for _ in 0..20 {
        original.push_str("helicopter landing pad -- ");
    }
    assert!(original.len() > 200, "expected long test text");

    search.index_file(path.clone(), original.clone());

    // "hello" does not occur as an exact substring, but shares the "hel" trigram with "helicopter".
    let results = search.search("hello");
    let result = results
        .iter()
        .find(|r| r.path == path)
        .expect("expected fuzzy trigram result for long.txt");

    assert_result_range(&original, "hello", result);
    assert!(
        result.range.end < original.len(),
        "expected prefix-window range for fuzzy match (not whole-file), got {:?} for len={}",
        result.range,
        original.len()
    );
}
