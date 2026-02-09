use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};

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
    /// Clear any indexed state.
    fn clear(&mut self) {}

    /// Add or replace a single file in the index.
    fn index_file(&mut self, _path: PathBuf, _text: String) {}

    /// Remove a single file from the index.
    fn remove_file(&mut self, _path: &Path) {}

    /// Index an entire project database.
    ///
    /// The default implementation rebuilds the index by calling [`SemanticSearch::clear`]
    /// followed by [`SemanticSearch::index_file`] for every file returned by
    /// [`ProjectDatabase::project_files`].
    fn index_project(&mut self, db: &dyn ProjectDatabase) {
        self.clear();
        for path in db.project_files() {
            let Some(text) = db.file_text(&path) else {
                continue;
            };
            self.index_file(path, text);
        }
    }

    /// Convenience helper to index a `nova_db::Database`.
    ///
    /// This avoids boilerplate wrapper code in callers by internally adapting
    /// `nova_db::Database` to [`ProjectDatabase`].
    fn index_database(&mut self, db: &dyn nova_db::Database) {
        let adapter = crate::project_database::DbProjectDatabase::new(db);
        self.index_project(&adapter);
    }

    /// Convenience helper to index a `nova_db::SourceDatabase`.
    fn index_source_database(&mut self, db: &dyn nova_db::SourceDatabase) {
        let adapter = crate::project_database::SourceDbProjectDatabase::new(db);
        self.index_project(&adapter);
    }

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
            let max_memory_bytes = config.embeddings.max_memory_bytes;
            match config.embeddings.backend {
                nova_config::AiEmbeddingsBackend::Hash => {
                    return Box::new(
                        EmbeddingSemanticSearch::new(HashEmbedder::default())
                            .with_max_memory_bytes(max_memory_bytes),
                    );
                }
                nova_config::AiEmbeddingsBackend::Provider => {
                    // Provider-backed embeddings are not guaranteed to be available for every
                    // configured provider kind. For now we fall back to the deterministic hash
                    // embedder so semantic search remains usable offline and in tests.
                    let provider_kind = &config.provider.kind;
                    let supports_provider_embeddings = matches!(
                        provider_kind,
                        nova_config::AiProviderKind::Ollama
                            | nova_config::AiProviderKind::OpenAiCompatible
                            | nova_config::AiProviderKind::OpenAi
                            | nova_config::AiProviderKind::Gemini
                            | nova_config::AiProviderKind::AzureOpenAi
                            | nova_config::AiProviderKind::Http
                    );

                    if supports_provider_embeddings {
                        tracing::warn!(
                            target = "nova.ai",
                            provider_kind = ?provider_kind,
                            "ai.embeddings.backend=provider is not implemented; falling back to hash embeddings"
                        );
                    } else {
                        tracing::warn!(
                            target = "nova.ai",
                            provider_kind = ?provider_kind,
                            "ai.embeddings.backend=provider is not supported for this ai.provider.kind; falling back to hash embeddings"
                        );
                    }

                    return Box::new(
                        EmbeddingSemanticSearch::new(HashEmbedder::default())
                            .with_max_memory_bytes(max_memory_bytes),
                    );
                }
                nova_config::AiEmbeddingsBackend::Local => {
                    tracing::warn!(
                        target = "nova.ai",
                        "ai.embeddings.backend=local is not implemented; falling back to hash embeddings"
                    );
                    return Box::new(
                        EmbeddingSemanticSearch::new(HashEmbedder::default())
                            .with_max_memory_bytes(max_memory_bytes),
                    );
                }
            }
        }

        #[cfg(not(feature = "embeddings"))]
        {
            tracing::warn!(
                target = "nova.ai",
                backend = ?config.embeddings.backend,
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
    docs: HashMap<PathBuf, IndexedDocument>,
}

#[derive(Debug)]
struct IndexedDocument {
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
    fn clear(&mut self) {
        self.docs.clear();
    }

    fn index_file(&mut self, path: PathBuf, text: String) {
        let (normalized, trigrams) = Self::index_text(&text);
        self.docs.insert(
            path,
            IndexedDocument {
                original: text,
                normalized,
                trigrams,
            },
        );
    }

    fn remove_file(&mut self, path: &Path) {
        self.docs.remove(path);
    }

    fn search(&self, query: &str) -> Vec<SearchResult> {
        let normalized_query = normalize(query);
        let query_trigrams = unique_sorted_trigrams(&normalized_query);

        let mut results: Vec<SearchResult> = self
            .docs
            .iter()
            .filter_map(|doc| {
                let (path, doc) = doc;
                let score = score_match(query, &normalized_query, &query_trigrams, path, doc);
                if score <= 0.0 {
                    return None;
                }

                Some(SearchResult {
                    path: path.clone(),
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
    path: &Path,
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
    let path_str = path.to_string_lossy();
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
    use crate::AiError;
    use nova_core::ProjectDatabase;
    use nova_fuzzy::{fuzzy_match, MatchKind};
    use std::cmp::Ordering;
    use std::collections::hash_map::DefaultHasher;
    use std::collections::BTreeMap;
    use std::hash::{Hash, Hasher};
    use std::ops::Range;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, OnceLock};

    use hnsw_rs::prelude::*;

    fn ensure_rayon_global_pool() {
        static INIT: OnceLock<()> = OnceLock::new();
        INIT.get_or_init(|| {
            // `hnsw_rs` uses rayon internally. Rayon's default global pool size is based on the
            // host CPU count, which can exceed thread and memory budgets in constrained
            // environments (CI sandboxes, editor/LSP test harnesses, etc.). Initialize the
            // global pool with a conservative thread count so embedding-backed semantic search
            // remains usable everywhere.
            let _ = rayon::ThreadPoolBuilder::new()
                .num_threads(1)
                .build_global();
        });
    }

    pub trait Embedder: Send + Sync {
        fn embed(&self, text: &str) -> Result<Vec<f32>, AiError>;

        fn embed_batch(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>, AiError> {
            inputs.iter().map(|input| self.embed(input)).collect()
        }
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
        fn embed(&self, text: &str) -> Result<Vec<f32>, AiError> {
            let mut vec = vec![0.0f32; self.dims];

            for token in tokenize(text) {
                let idx = (Self::token_hash(&token) % self.dims as u64) as usize;
                vec[idx] += 1.0;
            }

            l2_normalize(&mut vec);
            Ok(vec)
        }
    }

    #[derive(Debug, Clone)]
    struct EmbeddedDoc {
        range: Range<usize>,
        kind: String,
        snippet: String,
        embedding: Vec<f32>,
    }

    struct EmbeddedIndex {
        hnsw: Option<Hnsw<'static, f32, DistCosine>>,
        dims: usize,
        id_to_doc: Vec<(PathBuf, usize)>,
        dirty: bool,
        #[cfg(any(test, debug_assertions))]
        rebuild_count: usize,
    }

    impl EmbeddedIndex {
        fn empty() -> Self {
            Self {
                hnsw: None,
                dims: 0,
                id_to_doc: Vec::new(),
                dirty: false,
                #[cfg(any(test, debug_assertions))]
                rebuild_count: 0,
            }
        }
    }

    pub struct EmbeddingSemanticSearch<E: Embedder> {
        embedder: E,
        docs_by_path: BTreeMap<PathBuf, Vec<EmbeddedDoc>>,
        index: Mutex<EmbeddedIndex>,
        ef_search: usize,
        max_memory_bytes: Option<usize>,
        embedding_bytes_used: usize,
        truncation_warned: bool,
    }

    impl<E: Embedder> EmbeddingSemanticSearch<E> {
        pub fn new(embedder: E) -> Self {
            ensure_rayon_global_pool();
            Self {
                embedder,
                docs_by_path: BTreeMap::new(),
                index: Mutex::new(EmbeddedIndex::empty()),
                ef_search: 64,
                max_memory_bytes: None,
                embedding_bytes_used: 0,
                truncation_warned: false,
            }
        }

        pub fn with_ef_search(mut self, ef_search: usize) -> Self {
            self.ef_search = ef_search.max(1);
            self
        }

        pub fn with_max_memory_bytes(mut self, max_memory_bytes: usize) -> Self {
            // Treat `0` as a misconfiguration and clamp to something usable so indexing stays
            // deterministic.
            self.max_memory_bytes = Some(max_memory_bytes.max(1));
            self.enforce_memory_budget();
            self
        }

        fn embedding_bytes_for_docs(docs: &[EmbeddedDoc]) -> usize {
            docs.iter()
                .map(|doc| doc.embedding.len().saturating_mul(std::mem::size_of::<f32>()))
                .fold(0usize, usize::saturating_add)
        }

        fn warn_truncation_once(&mut self) {
            if self.truncation_warned {
                return;
            }
            self.truncation_warned = true;

            if let Some(limit) = self.max_memory_bytes {
                tracing::warn!(
                    target = "nova.ai",
                    max_memory_bytes = limit,
                    "embedding semantic search exceeded ai.embeddings.max_memory_bytes; truncating index to stay within budget"
                );
            } else {
                tracing::warn!(
                    target = "nova.ai",
                    "embedding semantic search truncated index to stay within memory budget"
                );
            }
        }

        /// Enforce the configured embedding memory budget by dropping whole-file entries until
        /// the estimate fits.
        ///
        /// This is best-effort: it estimates memory based on stored embedding vectors
        /// (`num_docs * dims * 4`), which is the dominant term for ANN indexes.
        fn enforce_memory_budget(&mut self) {
            let Some(limit) = self.max_memory_bytes else {
                return;
            };

            if self.embedding_bytes_used <= limit {
                return;
            }

            let mut changed = false;
            while self.embedding_bytes_used > limit {
                let Some((_path, docs)) = self.docs_by_path.pop_last() else {
                    self.embedding_bytes_used = 0;
                    break;
                };
                let removed_bytes = Self::embedding_bytes_for_docs(&docs);
                self.embedding_bytes_used = self.embedding_bytes_used.saturating_sub(removed_bytes);
                changed = true;
            }

            if changed {
                self.warn_truncation_once();
                self.invalidate_index();
            }
        }

        fn invalidate_index(&self) {
            let mut index = self.index.lock().expect("semantic search mutex poisoned");
            index.hnsw = None;
            index.dims = 0;
            index.id_to_doc.clear();
            index.dirty = true;
        }

        fn rebuild_index_locked(&self, index: &mut EmbeddedIndex) {
            #[cfg(any(test, debug_assertions))]
            {
                index.rebuild_count = index.rebuild_count.saturating_add(1);
            }

            index.hnsw = None;
            index.dims = 0;
            index.id_to_doc.clear();

            let max_elements = self
                .docs_by_path
                .values()
                .map(|docs| docs.len())
                .sum::<usize>();
            if max_elements == 0 {
                index.dirty = false;
                return;
            }

            let mut dims = 0usize;
            let mut hnsw: Option<Hnsw<'static, f32, DistCosine>> = None;

            let mut next_id = 0usize;
            for (path, docs) in &self.docs_by_path {
                for (local_idx, doc) in docs.iter().enumerate() {
                    if doc.embedding.is_empty() {
                        continue;
                    }

                    if dims == 0 {
                        dims = doc.embedding.len();
                        hnsw = Some(Hnsw::new(
                            /*max_nb_connection=*/ 16,
                            /*max_elements=*/ max_elements,
                            /*nb_layer=*/ 16,
                            /*ef_construction=*/ 200,
                            DistCosine {},
                        ));
                    }

                    if doc.embedding.len() != dims || dims == 0 {
                        continue;
                    }

                    if let Some(ref mut hnsw) = hnsw {
                        hnsw.insert((&doc.embedding, next_id));
                    }
                    index.id_to_doc.push((path.clone(), local_idx));
                    next_id += 1;
                }
            }

            if let Some(ref mut hnsw) = hnsw {
                hnsw.set_searching_mode(true);
            }

            index.hnsw = hnsw;
            index.dims = dims;
            index.dirty = false;
        }

        fn docs_for_file(
            &self,
            path: &PathBuf,
            text: &str,
        ) -> Vec<(Range<usize>, String, String, String)> {
            if text.is_empty() {
                return Vec::new();
            }

            let mut extracted = if is_java_file(path) {
                extract_java_methods(path, text)
            } else {
                extract_non_java_chunks(path, text)
            };

            if extracted.is_empty() {
                let preview = text.chars().take(2_000).collect::<String>();
                extracted.push((
                    0..text.len(),
                    "file".to_string(),
                    preview.clone(),
                    format!("{}\n{}", path.to_string_lossy(), preview),
                ));
            }

            extracted
        }
    }

    impl<E: Embedder> SemanticSearch for EmbeddingSemanticSearch<E> {
        fn clear(&mut self) {
            self.docs_by_path.clear();
            self.embedding_bytes_used = 0;
            let mut index = self.index.lock().expect("semantic search mutex poisoned");
            *index = EmbeddedIndex::empty();
        }

        fn index_project(&mut self, db: &dyn ProjectDatabase) {
            // Bulk indexing is often triggered during project open/refresh. Pre-build the HNSW
            // structure here so the first `search()` call doesn't pay the full rebuild cost.
            self.clear();
            for path in db.project_files() {
                let Some(text) = db.file_text(&path) else {
                    continue;
                };
                self.index_file(path, text);
            }

            let mut index = self.index.lock().expect("semantic search mutex poisoned");
            self.rebuild_index_locked(&mut index);
        }

        fn index_file(&mut self, path: PathBuf, text: String) {
            let mut changed = false;

            if let Some(existing) = self.docs_by_path.remove(&path) {
                let removed_bytes = Self::embedding_bytes_for_docs(&existing);
                self.embedding_bytes_used = self.embedding_bytes_used.saturating_sub(removed_bytes);
                changed = true;
            }

            let extracted = self.docs_for_file(&path, &text);
            if extracted.is_empty() {
                if changed {
                    self.invalidate_index();
                }
                return;
            }

            let mut meta = Vec::with_capacity(extracted.len());
            let mut inputs = Vec::with_capacity(extracted.len());
            for (range, kind, snippet, embed_text) in extracted {
                meta.push((range, kind, snippet));
                inputs.push(embed_text);
            }

            let embeddings = if inputs.len() <= 1 {
                match inputs.first() {
                    Some(input) => self.embedder.embed(input).map(|vec| vec![vec]),
                    None => Ok(Vec::new()),
                }
            } else {
                self.embedder.embed_batch(&inputs)
            };

            let embeddings = match embeddings {
                Ok(embeddings) => {
                    if embeddings.len() != meta.len() {
                        tracing::warn!(
                            target = "nova.ai",
                            path = %path.to_string_lossy(),
                            expected = meta.len(),
                            got = embeddings.len(),
                            "embedder returned unexpected batch size; skipping file"
                        );
                        Vec::new()
                    } else {
                        embeddings.into_iter().map(Some).collect::<Vec<_>>()
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        target = "nova.ai",
                        path = %path.to_string_lossy(),
                        ?err,
                        "failed to embed extracted docs; skipping failing docs"
                    );

                    // Best-effort fallback: attempt to embed each doc individually so partial
                    // failures don't wipe out the entire file.
                    let mut out = Vec::with_capacity(inputs.len());
                    for input in &inputs {
                        match self.embedder.embed(input) {
                            Ok(vec) => out.push(Some(vec)),
                            Err(err) => {
                                tracing::warn!(
                                    target = "nova.ai",
                                    path = %path.to_string_lossy(),
                                    ?err,
                                    "failed to embed doc"
                                );
                                out.push(None);
                            }
                        }
                    }
                    out
                }
            };
            let mut docs = Vec::new();
            for ((range, kind, snippet), embedding) in meta.into_iter().zip(embeddings) {
                let Some(mut embedding) = embedding else {
                    continue;
                };

                if embedding.is_empty() {
                    continue;
                }

                l2_normalize(&mut embedding);
                docs.push(EmbeddedDoc {
                    range,
                    kind,
                    snippet,
                    embedding,
                });
            }

            docs.sort_by(|a, b| {
                a.range
                    .start
                    .cmp(&b.range.start)
                    .then_with(|| a.range.end.cmp(&b.range.end))
                    .then_with(|| a.kind.cmp(&b.kind))
            });

            if docs.is_empty() {
                if changed {
                    self.invalidate_index();
                }
                return;
            }

            // Enforce a soft memory budget based on the stored embedding vectors. When truncating,
            // keep the earliest ranges first so behavior is deterministic.
            if let Some(limit) = self.max_memory_bytes {
                self.enforce_memory_budget();

                let remaining = limit.saturating_sub(self.embedding_bytes_used);
                let original_len = docs.len();
                let mut kept = Vec::new();
                let mut kept_bytes = 0usize;

                for doc in docs {
                    let doc_bytes =
                        doc.embedding
                            .len()
                            .saturating_mul(std::mem::size_of::<f32>());
                    let next = kept_bytes.saturating_add(doc_bytes);
                    if next > remaining {
                        break;
                    }
                    kept_bytes = next;
                    kept.push(doc);
                }

                if kept.len() != original_len {
                    self.warn_truncation_once();
                }

                if kept.is_empty() {
                    if changed {
                        self.invalidate_index();
                    }
                    return;
                }

                docs = kept;
                self.embedding_bytes_used = self.embedding_bytes_used.saturating_add(kept_bytes);
            } else {
                self.embedding_bytes_used = self
                    .embedding_bytes_used
                    .saturating_add(Self::embedding_bytes_for_docs(&docs));
            }

            self.docs_by_path.insert(path, docs);
            changed = true;
            if changed {
                self.invalidate_index();
            }
        }

        fn remove_file(&mut self, path: &Path) {
            if let Some(removed) = self.docs_by_path.remove(path) {
                let removed_bytes = Self::embedding_bytes_for_docs(&removed);
                self.embedding_bytes_used = self.embedding_bytes_used.saturating_sub(removed_bytes);
                self.invalidate_index();
            }
        }

        fn search(&self, query: &str) -> Vec<SearchResult> {
            let mut query_embedding = match self.embedder.embed(query) {
                Ok(embedding) => embedding,
                Err(err) => {
                    tracing::warn!(
                        target = "nova.ai",
                        ?err,
                        "failed to embed query; returning empty results"
                    );
                    return Vec::new();
                }
            };

            if query_embedding.is_empty() {
                return Vec::new();
            }
            l2_normalize(&mut query_embedding);

            let query_substring = query.trim();
            let query_substring_lower = query_substring.to_lowercase();

            let mut index = self.index.lock().expect("semantic search mutex poisoned");
            if index.dirty {
                self.rebuild_index_locked(&mut index);
            }

            let Some(hnsw) = &index.hnsw else {
                return Vec::new();
            };

            if query_embedding.len() != index.dims || index.dims == 0 {
                return Vec::new();
            }

            let mut results = Vec::new();

            let neighbours = hnsw.search(&query_embedding, 50, self.ef_search);
            for n in neighbours {
                let Some((path, local_idx)) = index.id_to_doc.get(n.d_id) else {
                    continue;
                };
                let Some(docs) = self.docs_by_path.get(path) else {
                    continue;
                };
                let Some(doc) = docs.get(*local_idx) else {
                    continue;
                };

                // Re-score with exact cosine similarity for deterministic ranking.
                let mut score = cosine_similarity(&query_embedding, &doc.embedding);

                const SUBSTRING_MATCH_BOOST: f32 = 0.05;
                const MAX_LEXICAL_BOOST: f32 = 0.20;
                let mut lexical_boost = 0.0f32;

                if !query_substring_lower.is_empty()
                    && doc.snippet.to_lowercase().contains(&query_substring_lower)
                {
                    lexical_boost += SUBSTRING_MATCH_BOOST;
                }

                let path_str = path.to_string_lossy();
                if let Some(score_path) = fuzzy_match(query, &path_str) {
                    lexical_boost += match score_path.kind {
                        MatchKind::Prefix => 0.15,
                        MatchKind::Fuzzy => 0.05,
                    };
                }

                score += lexical_boost.min(MAX_LEXICAL_BOOST);
                results.push(SearchResult {
                    path: path.clone(),
                    range: doc.range.clone(),
                    kind: doc.kind.clone(),
                    score,
                    snippet: doc.snippet.clone(),
                });
            }

            results.sort_by(|a, b| {
                match b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal) {
                    Ordering::Equal => {
                        let by_path = a.path.cmp(&b.path);
                        if by_path != Ordering::Equal {
                            return by_path;
                        }

                        let by_start = a.range.start.cmp(&b.range.start);
                        if by_start != Ordering::Equal {
                            return by_start;
                        }

                        let by_end = a.range.end.cmp(&b.range.end);
                        if by_end != Ordering::Equal {
                            return by_end;
                        }

                        a.kind.cmp(&b.kind)
                    }
                    other => other,
                }
            });
            results.truncate(50);
            results
        }
    }

    // -----------------------------------------------------------------------------
    // Test / debug-only helpers
    // -----------------------------------------------------------------------------

    #[cfg(any(test, debug_assertions))]
    impl<E: Embedder> EmbeddingSemanticSearch<E> {
        #[doc(hidden)]
        pub fn __index_is_dirty_for_tests(&self) -> bool {
            self.index
                .lock()
                .expect("semantic search mutex poisoned")
                .dirty
        }

        #[doc(hidden)]
        pub fn __index_rebuild_count_for_tests(&self) -> usize {
            self.index
                .lock()
                .expect("semantic search mutex poisoned")
                .rebuild_count
        }
    }

    fn is_java_file(path: &Path) -> bool {
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.eq_ignore_ascii_case("java"))
            .unwrap_or(false)
    }

    /// Deterministically chunk non-Java files so semantic search can return meaningful
    /// snippets + ranges without language-specific parsers.
    ///
    /// This intentionally only embeds the first few chunks to avoid runaway indexing
    /// costs for very large documentation/config files.
    fn extract_non_java_chunks(
        path: &PathBuf,
        text: &str,
    ) -> Vec<(Range<usize>, String, String, String)> {
        const CHUNK_SIZE_CHARS: usize = 1_000;
        const CHUNK_OVERLAP_CHARS: usize = 100;
        const MAX_CHUNKS: usize = 8;

        // Keep the existing behaviour for small files: embed as a single document so
        // we don't introduce additional docs for tiny configs.
        if is_very_small_file(text, CHUNK_SIZE_CHARS) {
            return Vec::new();
        }

        let stride = CHUNK_SIZE_CHARS.saturating_sub(CHUNK_OVERLAP_CHARS).max(1);
        let mut out = Vec::new();
        let mut start_char = 0usize;

        for _ in 0..MAX_CHUNKS {
            let start = byte_offset_for_char(text, start_char);
            if start >= text.len() {
                break;
            }

            let end = byte_offset_for_char(text, start_char.saturating_add(CHUNK_SIZE_CHARS));
            let end = end.min(text.len()).max(start);
            if end == start {
                break;
            }

            // Range indices are guaranteed to be UTF-8 boundaries because they're derived
            // from `char_indices`.
            let snippet = text[start..end].to_string();
            let embed_text = format!("{}\n{}", path.to_string_lossy(), snippet);
            out.push((start..end, "chunk".to_string(), snippet, embed_text));

            if end >= text.len() {
                break;
            }

            start_char = start_char.saturating_add(stride);
        }

        out
    }

    fn extract_java_methods(
        path: &PathBuf,
        source: &str,
    ) -> Vec<(Range<usize>, String, String, String)> {
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

            out.push((start..end, "method".to_string(), snippet, embed_text));
        }

        out
    }

    fn preview(text: &str, max_chars: usize) -> String {
        text.chars().take(max_chars).collect()
    }

    fn is_very_small_file(text: &str, chunk_size_chars: usize) -> bool {
        // Avoid `text.chars().count()` because files can be large; we only need to know if the
        // file exceeds our chunking threshold.
        text.chars().take(chunk_size_chars + 1).count() <= chunk_size_chars
    }

    fn byte_offset_for_char(text: &str, char_idx: usize) -> usize {
        if char_idx == 0 {
            return 0;
        }

        text.char_indices()
            .nth(char_idx)
            .map(|(idx, _)| idx)
            .unwrap_or_else(|| text.len())
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
