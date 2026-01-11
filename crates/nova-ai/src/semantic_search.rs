use std::cmp::Ordering;
use std::collections::HashSet;
use std::ops::Range;
use std::path::PathBuf;

use nova_core::ProjectDatabase;
use nova_fuzzy::{fuzzy_match, MatchKind};

/// A single semantic search match.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchResult {
    pub path: PathBuf,
    pub range: Range<usize>,
    pub kind: String,
    pub score: f32,
    pub snippet: String,
}

/// Semantic search interface.
///
/// When built with the `embeddings` Cargo feature, `nova-ai` includes a local
/// embedding-backed implementation. Without it, the crate falls back to a
/// lightweight trigram/fuzzy matcher so semantic search remains available
/// without any model dependencies.
pub trait SemanticSearch: Send + Sync {
    fn index_project(&mut self, db: &dyn ProjectDatabase);
    fn search(&self, query: &str) -> Vec<SearchResult>;
}

/// Construct a [`SemanticSearch`] implementation from runtime configuration.
///
/// If `ai.embeddings.enabled = true` and the crate is compiled with the
/// `embeddings` Cargo feature, this returns an [`EmbeddingSemanticSearch`]
/// instance backed by a lightweight local embedder.
///
/// When embeddings are enabled in config but the crate is built without the
/// `embeddings` feature, this falls back to [`TrigramSemanticSearch`].
pub fn semantic_search_from_config(config: &nova_config::AiConfig) -> Box<dyn SemanticSearch> {
    if !(config.enabled && config.features.semantic_search) {
        return Box::new(NoopSemanticSearch);
    }

    if config.embeddings.enabled {
        #[cfg(feature = "embeddings")]
        {
            return Box::new(EmbeddingSemanticSearch::new(HashEmbedder::default()));
        }

        #[cfg(not(feature = "embeddings"))]
        {
            tracing::warn!(
                target = "nova.ai",
                "ai.embeddings.enabled is true but nova-ai was built without the `embeddings` feature; falling back to trigram search"
            );
        }
    }

    Box::new(TrigramSemanticSearch::new())
}

#[derive(Debug, Default)]
pub struct NoopSemanticSearch;

impl SemanticSearch for NoopSemanticSearch {
    fn index_project(&mut self, _db: &dyn ProjectDatabase) {}

    fn search(&self, _query: &str) -> Vec<SearchResult> {
        Vec::new()
    }
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
                    range: 0..doc.original.len(),
                    kind: "file".to_string(),
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

#[cfg(feature = "embeddings")]
mod embeddings {
    use super::{SearchResult, SemanticSearch};
    use nova_core::ProjectDatabase;
    use nova_fuzzy::{fuzzy_match, MatchKind};
    use std::cmp::Ordering;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::ops::Range;
    use std::path::{Path, PathBuf};

    use hnsw_rs::prelude::*;

    pub trait Embedder: Send + Sync {
        fn embed(&self, text: &str) -> Vec<f32>;
    }

    /// A lightweight, fully-local embedder based on the hashing trick.
    ///
    /// This is not a neural embedding model, but it provides a stable vector
    /// representation suitable for ANN indexes and offline tests.
    #[derive(Debug, Clone)]
    pub struct HashEmbedder {
        dims: usize,
    }

    impl HashEmbedder {
        pub fn new(dims: usize) -> Self {
            Self { dims: dims.max(1) }
        }

        fn token_hash(token: &str) -> u64 {
            let mut hasher = DefaultHasher::new();
            token.hash(&mut hasher);
            hasher.finish()
        }
    }

    impl Default for HashEmbedder {
        fn default() -> Self {
            Self::new(256)
        }
    }

    impl Embedder for HashEmbedder {
        fn embed(&self, text: &str) -> Vec<f32> {
            let mut vec = vec![0.0f32; self.dims];

            for token in tokenize(text) {
                let idx = (Self::token_hash(&token) % self.dims as u64) as usize;
                vec[idx] += 1.0;
            }

            l2_normalize(&mut vec);
            vec
        }
    }

    #[derive(Debug, Clone)]
    struct EmbeddedDoc {
        path: PathBuf,
        range: Range<usize>,
        kind: String,
        snippet: String,
        embedding: Vec<f32>,
    }

    pub struct EmbeddingSemanticSearch<E: Embedder> {
        embedder: E,
        docs: Vec<EmbeddedDoc>,
        hnsw: Option<Hnsw<'static, f32, DistCosine>>,
        dims: usize,
        ef_search: usize,
    }

    impl<E: Embedder> EmbeddingSemanticSearch<E> {
        pub fn new(embedder: E) -> Self {
            Self {
                embedder,
                docs: Vec::new(),
                hnsw: None,
                dims: 0,
                ef_search: 64,
            }
        }

        pub fn with_ef_search(mut self, ef_search: usize) -> Self {
            self.ef_search = ef_search.max(1);
            self
        }
    }

    impl<E: Embedder> SemanticSearch for EmbeddingSemanticSearch<E> {
        fn index_project(&mut self, db: &dyn ProjectDatabase) {
            self.docs.clear();
            self.hnsw = None;
            self.dims = 0;

            let mut pending: Vec<(PathBuf, Range<usize>, String, String, String)> = Vec::new();

            for path in db.project_files() {
                let Some(text) = db.file_text(&path) else {
                    continue;
                };
                if text.is_empty() {
                    continue;
                }

                let mut extracted = if is_java_file(&path) {
                    extract_java_methods(&path, &text)
                } else {
                    Vec::new()
                };

                if extracted.is_empty() {
                    // Fallback to file-level indexing.
                    let preview = text.chars().take(2_000).collect::<String>();
                    extracted.push((
                        path.clone(),
                        0..text.len(),
                        "file".to_string(),
                        preview.clone(),
                        format!("{}\n{}", path.to_string_lossy(), preview),
                    ));
                }

                pending.extend(extracted);
            }

            if pending.is_empty() {
                return;
            }

            // HNSW needs a fixed dimensionality, so we discover it from the first non-empty embedding.
            let max_elements = pending.len();
            let mut hnsw: Option<Hnsw<'static, f32, DistCosine>> = None;

            for (path, range, kind, snippet, embed_text) in pending {
                let mut embedding = self.embedder.embed(&embed_text);
                if embedding.is_empty() {
                    continue;
                }
                l2_normalize(&mut embedding);

                if self.dims == 0 {
                    self.dims = embedding.len();
                    hnsw = Some(Hnsw::new(
                        /*max_nb_connection=*/ 16,
                        /*max_elements=*/ max_elements,
                        /*nb_layer=*/ 16,
                        /*ef_construction=*/ 200,
                        DistCosine {},
                    ));
                }

                if embedding.len() != self.dims {
                    continue;
                }

                let id = self.docs.len();
                if let Some(ref mut hnsw) = hnsw {
                    hnsw.insert((&embedding, id));
                }

                self.docs.push(EmbeddedDoc {
                    path,
                    range,
                    kind,
                    snippet,
                    embedding,
                });
            }

            if let Some(ref mut hnsw) = hnsw {
                // HNSW is not safe to mutate concurrently with search; enter a
                // dedicated searching mode after building the index.
                hnsw.set_searching_mode(true);
            }
            self.hnsw = hnsw;
        }

        fn search(&self, query: &str) -> Vec<SearchResult> {
            let Some(hnsw) = &self.hnsw else {
                return Vec::new();
            };

            let mut query_embedding = self.embedder.embed(query);
            if query_embedding.is_empty() {
                return Vec::new();
            }
            l2_normalize(&mut query_embedding);
            if query_embedding.len() != self.dims || self.dims == 0 {
                return Vec::new();
            }

            let mut results = Vec::new();

            let neighbours = hnsw.search(&query_embedding, 50, self.ef_search);
            for n in neighbours {
                let Some(doc) = self.docs.get(n.d_id) else {
                    continue;
                };
                // Re-score with exact cosine similarity for deterministic ranking.
                let mut score = cosine_similarity(&query_embedding, &doc.embedding);
                let path_str = doc.path.to_string_lossy();
                if let Some(score_path) = fuzzy_match(query, &path_str) {
                    score += match score_path.kind {
                        MatchKind::Prefix => 0.15,
                        MatchKind::Fuzzy => 0.05,
                    };
                }
                results.push(SearchResult {
                    path: doc.path.clone(),
                    range: doc.range.clone(),
                    kind: doc.kind.clone(),
                    score,
                    snippet: doc.snippet.clone(),
                });
            }

            results.sort_by(|a, b| {
                match b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal) {
                    Ordering::Equal => a.path.cmp(&b.path),
                    other => other,
                }
            });
            results.truncate(50);
            results
        }
    }

    fn is_java_file(path: &Path) -> bool {
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.eq_ignore_ascii_case("java"))
            .unwrap_or(false)
    }

    fn extract_java_methods(
        path: &PathBuf,
        source: &str,
    ) -> Vec<(PathBuf, Range<usize>, String, String, String)> {
        use nova_syntax::{parse_java, SyntaxKind};

        let parse = parse_java(source);
        let root = parse.syntax();
        let mut out = Vec::new();

        for node in root.descendants() {
            if node.kind() != SyntaxKind::MethodDeclaration {
                continue;
            }

            let range = node.text_range();
            let start = u32::from(range.start()) as usize;
            let end = u32::from(range.end()) as usize;
            let start = start.min(source.len());
            let end = end.min(source.len()).max(start);
            let method_text = source[start..end].trim();
            if method_text.is_empty() {
                continue;
            }

            let doc = find_doc_comment_before_offset(source, start).unwrap_or_default();
            let signature = extract_signature(method_text);
            let body = extract_body_preview(method_text);
            let snippet = preview(method_text, 2_000);
            let embed_text = format!("{}\n{doc}\n{signature}\n{body}", path.to_string_lossy());

            out.push((
                path.clone(),
                start..end,
                "method".to_string(),
                snippet,
                embed_text,
            ));
        }

        out
    }

    fn preview(text: &str, max_chars: usize) -> String {
        text.chars().take(max_chars).collect()
    }

    fn extract_signature(snippet: &str) -> String {
        let end = snippet
            .find('{')
            .or_else(|| snippet.find(';'))
            .unwrap_or(snippet.len());
        snippet[..end].trim().to_string()
    }

    fn extract_body_preview(snippet: &str) -> String {
        let Some(pos) = snippet.find('{') else {
            return String::new();
        };
        snippet[pos + 1..]
            .chars()
            .take(500)
            .collect::<String>()
            .trim()
            .to_string()
    }

    fn find_doc_comment_before_offset(source: &str, offset: usize) -> Option<String> {
        use nova_syntax::SyntaxKind;

        let tokens = nova_syntax::lex(source);
        let mut idx = 0usize;
        while idx < tokens.len() {
            let end = tokens[idx].range.end as usize;
            if end > offset {
                break;
            }
            idx += 1;
        }

        while idx > 0 {
            idx -= 1;
            let tok = &tokens[idx];
            match tok.kind {
                SyntaxKind::Whitespace | SyntaxKind::LineComment | SyntaxKind::BlockComment => {
                    continue
                }
                SyntaxKind::DocComment => return Some(tok.text(source).to_string()),
                _ => break,
            }
        }

        None
    }

    fn tokenize(text: &str) -> Vec<String> {
        let mut tokens = Vec::new();
        let mut current = String::new();
        let mut prev_is_lower = false;

        for ch in text.chars() {
            if ch.is_ascii_alphanumeric() {
                let is_upper = ch.is_ascii_uppercase();
                if is_upper && prev_is_lower && !current.is_empty() {
                    tokens.push(current.clone());
                    current.clear();
                }
                current.push(ch.to_ascii_lowercase());
                prev_is_lower = ch.is_ascii_lowercase();
            } else {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
                prev_is_lower = false;
            }
        }

        if !current.is_empty() {
            tokens.push(current);
        }

        tokens
    }

    fn l2_normalize(vec: &mut [f32]) {
        let norm: f32 = vec.iter().map(|v| v * v).sum::<f32>().sqrt();
        if norm > 0.0 {
            for v in vec {
                *v /= norm;
            }
        }
    }

    fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
        let n = a.len().min(b.len());
        let mut dot = 0.0f32;
        for i in 0..n {
            dot += a[i] * b[i];
        }
        dot
    }
}

#[cfg(feature = "embeddings")]
pub use embeddings::{Embedder, EmbeddingSemanticSearch, HashEmbedder};
