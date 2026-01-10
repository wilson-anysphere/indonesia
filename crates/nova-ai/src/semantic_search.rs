use std::cmp::Ordering;
use std::collections::HashSet;
use std::path::PathBuf;

use nova_core::ProjectDatabase;
use nova_fuzzy::{fuzzy_match, MatchKind};

/// A single semantic search match.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchResult {
    pub path: PathBuf,
    pub score: f32,
    pub snippet: String,
}

/// Semantic search interface.
///
/// The full Nova system will likely provide an embedding-backed implementation.
/// This crate includes a trigram/fuzzy stub so the functionality exists without
/// models.
pub trait SemanticSearch: Send + Sync {
    fn index_project(&mut self, db: &dyn ProjectDatabase);
    fn search(&self, query: &str) -> Vec<SearchResult>;
}

#[derive(Debug, Default)]
pub struct TrigramSemanticSearch {
    docs: Vec<IndexedDocument>,
}

#[derive(Debug)]
struct IndexedDocument {
    path: PathBuf,
    original: String,
    normalized: String,
    trigrams: Vec<u32>,
}

impl TrigramSemanticSearch {
    pub fn new() -> Self {
        Self::default()
    }

    fn index_text(text: &str) -> (String, Vec<u32>) {
        let normalized = normalize(text);
        let trigrams = unique_sorted_trigrams(&normalized);
        (normalized, trigrams)
    }
}

impl SemanticSearch for TrigramSemanticSearch {
    fn index_project(&mut self, db: &dyn ProjectDatabase) {
        self.docs.clear();

        for path in db.project_files() {
            let Some(text) = db.file_text(&path) else {
                continue;
            };

            let (normalized, trigrams) = Self::index_text(&text);
            self.docs.push(IndexedDocument {
                path,
                original: text,
                normalized,
                trigrams,
            });
        }
    }

    fn search(&self, query: &str) -> Vec<SearchResult> {
        let normalized_query = normalize(query);
        let query_trigrams = unique_sorted_trigrams(&normalized_query);

        let mut results: Vec<SearchResult> = self
            .docs
            .iter()
            .filter_map(|doc| {
                let score = score_match(query, &normalized_query, &query_trigrams, doc);
                if score <= 0.0 {
                    return None;
                }

                Some(SearchResult {
                    path: doc.path.clone(),
                    score,
                    snippet: snippet(&doc.original, &doc.normalized, &normalized_query),
                })
            })
            .collect();

        results.sort_by(
            |a, b| match b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal) {
                Ordering::Equal => a.path.cmp(&b.path),
                other => other,
            },
        );

        results.truncate(50);
        results
    }
}

fn normalize(text: &str) -> String {
    let mut out = Vec::with_capacity(text.len());
    for &b in text.as_bytes() {
        let folded = b.to_ascii_lowercase();
        if folded.is_ascii_alphanumeric() {
            out.push(folded);
        } else {
            out.push(b' ');
        }
    }
    // Safe because `out` only contains ASCII bytes.
    String::from_utf8(out).unwrap_or_default()
}

fn unique_sorted_trigrams(text: &str) -> Vec<u32> {
    let bytes = text.as_bytes();
    if bytes.len() < 3 {
        return Vec::new();
    }

    let mut set: HashSet<u32> = HashSet::new();
    for window in bytes.windows(3) {
        let tri = (window[0] as u32) | ((window[1] as u32) << 8) | ((window[2] as u32) << 16);
        set.insert(tri);
    }

    let mut out: Vec<u32> = set.into_iter().collect();
    out.sort_unstable();
    out
}

fn trigram_jaccard(a: &[u32], b: &[u32]) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }

    let mut i = 0usize;
    let mut j = 0usize;
    let mut intersection = 0usize;

    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            Ordering::Equal => {
                intersection += 1;
                i += 1;
                j += 1;
            }
            Ordering::Less => i += 1,
            Ordering::Greater => j += 1,
        }
    }

    let union = a.len() + b.len() - intersection;
    if union == 0 {
        0.0
    } else {
        intersection as f32 / union as f32
    }
}

fn score_match(
    raw_query: &str,
    normalized_query: &str,
    query_trigrams: &[u32],
    doc: &IndexedDocument,
) -> f32 {
    if normalized_query.is_empty() {
        return 0.0;
    }

    let mut score = trigram_jaccard(query_trigrams, &doc.trigrams);

    // Boost exact substring matches (after normalization).
    if doc.normalized.contains(normalized_query) {
        score += 0.75;
    }

    // A small boost if the query matches the file path.
    let path_str = doc.path.to_string_lossy();
    if let Some(score_path) = fuzzy_match(raw_query, &path_str) {
        score += match score_path.kind {
            MatchKind::Prefix => 0.25,
            MatchKind::Fuzzy => 0.1,
        };
    }

    score
}

fn snippet(original: &str, normalized: &str, query: &str) -> String {
    if query.is_empty() {
        return String::new();
    }

    if let Some(pos) = normalized.find(query) {
        let mut start = pos.saturating_sub(30);
        let mut end = (pos + query.len() + 30).min(original.len());

        while start > 0 && !original.is_char_boundary(start) {
            start -= 1;
        }
        while end < original.len() && !original.is_char_boundary(end) {
            end += 1;
        }

        return original[start..end].trim().to_string();
    }

    original
        .chars()
        .take(80)
        .collect::<String>()
        .trim()
        .to_string()
}
