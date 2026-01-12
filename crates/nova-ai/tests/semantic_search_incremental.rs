use std::path::PathBuf;

use nova_ai::{SemanticSearch, TrigramSemanticSearch, VirtualWorkspace};

#[test]
fn trigram_index_file_then_search_returns_results() {
    let mut search = TrigramSemanticSearch::new();
    search.index_file(
        PathBuf::from("src/Hello.java"),
        "public class Hello { String hello() { return \"hello\"; } }".to_string(),
    );

    let results = search.search("hello");
    assert!(!results.is_empty());
    assert_eq!(results[0].path, PathBuf::from("src/Hello.java"));
    assert!(results[0].snippet.to_lowercase().contains("hello"));
}

#[test]
fn trigram_upsert_updates_ranking_and_snippet() {
    let mut search = TrigramSemanticSearch::new();
    let path_a = PathBuf::from("src/a.txt");
    let path_b = PathBuf::from("src/b.txt");

    // Path A is initially the strongest match because it contains "hello" as a substring.
    search.index_file(
        path_a.clone(),
        "hello abcdefghijklmnopqrstuvwxyz".to_string(),
    );
    search.index_file(path_b.clone(), "helicopter landing pad".to_string());

    let before = search.search("hello");
    assert_eq!(before[0].path, path_a);

    let before_b = before
        .iter()
        .find(|r| r.path == path_b)
        .expect("expected b.txt to have a non-zero trigram score")
        .snippet
        .clone();

    // Upsert B with content that matches the query exactly; B should now outrank A.
    search.index_file(path_b.clone(), "hello".to_string());

    let after = search.search("hello");
    assert_eq!(after[0].path, path_b);
    assert_ne!(after[0].snippet, before_b);
    assert!(after[0].snippet.to_lowercase().contains("hello"));
}

#[test]
fn trigram_remove_file_removes_from_results() {
    let mut search = TrigramSemanticSearch::new();
    let path_a = PathBuf::from("src/a.txt");
    let path_b = PathBuf::from("src/b.txt");

    search.index_file(path_a.clone(), "hello world".to_string());
    search.index_file(path_b.clone(), "hello there".to_string());
    assert!(search.search("hello").iter().any(|r| r.path == path_a));

    search.remove_file(path_a.as_path());
    let results = search.search("hello");
    assert!(!results.iter().any(|r| r.path == path_a));
    assert!(results.iter().any(|r| r.path == path_b));
}

#[test]
fn trigram_index_project_matches_repeated_index_file() {
    let files = vec![("src/a.txt", "hello world"), ("src/b.txt", "helicopter landing pad")];

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
