use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::AiError;
use nova_core::ProjectDatabase;
use nova_fuzzy::{fuzzy_match, MatchKind};
use nova_metrics::MetricsRegistry;

const AI_SEMANTIC_SEARCH_SEARCH_METRIC: &str = "ai/semantic_search/search";
const AI_SEMANTIC_SEARCH_INDEX_FILE_METRIC: &str = "ai/semantic_search/index_file";
const AI_SEMANTIC_SEARCH_INDEX_PROJECT_METRIC: &str = "ai/semantic_search/index_project";
const AI_SEMANTIC_SEARCH_FINALIZE_INDEXING_METRIC: &str = "ai/semantic_search/finalize_indexing";

#[cfg(feature = "embeddings")]
fn ai_error_is_timeout(err: &AiError) -> bool {
    matches!(err, AiError::Timeout) || matches!(err, AiError::Http(http_err) if http_err.is_timeout())
}

#[cfg(feature = "embeddings")]
fn record_semantic_search_failure(metric: &str, err: &AiError) {
    let registry = MetricsRegistry::global();
    if ai_error_is_timeout(err) {
        registry.record_timeout(metric);
    } else {
        registry.record_error(metric);
    }
}

struct MetricGuard {
    metric: &'static str,
    started_at: Instant,
}

impl MetricGuard {
    fn new(metric: &'static str) -> Self {
        Self {
            metric,
            started_at: Instant::now(),
        }
    }
}

impl Drop for MetricGuard {
    fn drop(&mut self) {
        let metrics = MetricsRegistry::global();
        metrics.record_request(self.metric, self.started_at.elapsed());
        if std::thread::panicking() {
            metrics.record_panic(self.metric);
        }
    }
}

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
/// When built with the `embeddings` Cargo feature, `nova-ai` includes an
/// embedding-backed implementation. By default this uses the hashing trick
/// (`HashEmbedder`) so semantic search remains fully offline and deterministic.
///
/// When additionally built with the `embeddings-local` Cargo feature, callers
/// can opt into true in-process neural embeddings via
/// `ai.embeddings.backend = "local"`.
///
/// Without the `embeddings` feature, the crate falls back to a lightweight
/// trigram/fuzzy matcher so semantic search remains available without any model
/// dependencies.
pub trait SemanticSearch: Send + Sync {
    /// Clear any indexed state.
    fn clear(&mut self) {}

    /// Add or replace a single file in the index.
    fn index_file(&mut self, _path: PathBuf, _text: String) {}

    /// Remove a single file from the index.
    fn remove_file(&mut self, _path: &Path) {}

    /// Finalize any pending indexing work after a bulk update.
    ///
    /// Some [`SemanticSearch`] implementations keep auxiliary data structures (for example, an
    /// embedding-backed ANN index) in a "dirty" state while [`SemanticSearch::index_file`] is
    /// called repeatedly, and rebuild lazily on the first [`SemanticSearch::search`]. Call this
    /// method after a bulk indexing loop to make the first search fast.
    ///
    /// The default implementation is a no-op.
    fn finalize_indexing(&self) {
        let _guard = MetricGuard::new(AI_SEMANTIC_SEARCH_FINALIZE_INDEXING_METRIC);
    }

    /// Index an entire project database.
    ///
    /// The default implementation rebuilds the index by calling [`SemanticSearch::clear`]
    /// followed by [`SemanticSearch::index_file`] for every file returned by
    /// [`ProjectDatabase::project_files`].
    fn index_project(&mut self, db: &dyn ProjectDatabase) {
        let _guard = MetricGuard::new(AI_SEMANTIC_SEARCH_INDEX_PROJECT_METRIC);
        self.clear();
        for path in db.project_files() {
            let Some(text) = db.file_text(&path) else {
                continue;
            };
            self.index_file(path, text);
        }
        self.finalize_indexing();
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
/// When `ai.embeddings.enabled = true` and the crate is compiled with the
/// `embeddings` Cargo feature, this returns an [`EmbeddingSemanticSearch`]
/// instance backed by the configured embeddings backend (`hash`, `provider`,
/// or `local`).
///
/// When embeddings are enabled in config but the crate is built without the
/// `embeddings` feature, this falls back to [`TrigramSemanticSearch`].
pub fn semantic_search_from_config(
    config: &nova_config::AiConfig,
) -> Result<Box<dyn SemanticSearch>, AiError> {
    if !(config.enabled && config.features.semantic_search) {
        return Ok(Box::new(NoopSemanticSearch));
    }

    if config.embeddings.enabled {
        #[cfg(feature = "embeddings")]
        {
            if config.embeddings.model_dir.as_os_str().is_empty() {
                return Err(AiError::InvalidConfig(
                    "ai.embeddings.model_dir must be non-empty when ai.embeddings.enabled=true"
                        .to_string(),
                ));
            }

            std::fs::create_dir_all(&config.embeddings.model_dir).map_err(|err| {
                AiError::InvalidConfig(format!(
                    "failed to create ai.embeddings.model_dir {}: {err}",
                    config.embeddings.model_dir.display()
                ))
            })?;

            let max_memory_bytes =
                (config.embeddings.max_memory_bytes.0).min(usize::MAX as u64) as usize;

            let search = match config.embeddings.backend {
                nova_config::AiEmbeddingsBackend::Hash => EmbeddingSemanticSearch::new(
                    HashEmbedder::default(),
                )
                .with_max_memory_bytes(max_memory_bytes),
                nova_config::AiEmbeddingsBackend::Provider => {
                    if let Some(embedder) = embeddings::provider_embedder_from_config(config) {
                        return Ok(Box::new(
                            EmbeddingSemanticSearch::new(embedder)
                                .with_max_memory_bytes(max_memory_bytes),
                        ));
                    }

                    EmbeddingSemanticSearch::new(HashEmbedder::default())
                        .with_max_memory_bytes(max_memory_bytes)
                }
                nova_config::AiEmbeddingsBackend::Local => {
                    #[cfg(feature = "embeddings-local")]
                    {
                        match LocalEmbedder::from_config(&config.embeddings) {
                            Ok(embedder) => {
                                return Ok(Box::new(
                                    EmbeddingSemanticSearch::new(embedder)
                                        .with_max_memory_bytes(max_memory_bytes),
                                ));
                            }
                            Err(err) => {
                                tracing::warn!(
                                    target = "nova.ai",
                                    ?err,
                                    "failed to initialize local embeddings; falling back to hash embeddings"
                                );
                            }
                        }
                    }

                    #[cfg(not(feature = "embeddings-local"))]
                    {
                        tracing::warn!(
                            target = "nova.ai",
                            "ai.embeddings.backend=local but nova-ai was built without the `embeddings-local` feature; falling back to hash embeddings"
                        );
                    }

                    EmbeddingSemanticSearch::new(HashEmbedder::default())
                        .with_max_memory_bytes(max_memory_bytes)
                }
            };

            return Ok(Box::new(search));
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

    Ok(Box::new(TrigramSemanticSearch::new()))
}

#[derive(Debug, Default)]
pub struct NoopSemanticSearch;

impl SemanticSearch for NoopSemanticSearch {
    fn index_project(&mut self, _db: &dyn ProjectDatabase) {
        let _guard = MetricGuard::new(AI_SEMANTIC_SEARCH_INDEX_PROJECT_METRIC);
    }

    fn index_file(&mut self, _path: PathBuf, _text: String) {
        let _guard = MetricGuard::new(AI_SEMANTIC_SEARCH_INDEX_FILE_METRIC);
    }

    fn search(&self, _query: &str) -> Vec<SearchResult> {
        let _guard = MetricGuard::new(AI_SEMANTIC_SEARCH_SEARCH_METRIC);
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
        let _guard = MetricGuard::new(AI_SEMANTIC_SEARCH_INDEX_FILE_METRIC);
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
        let _guard = MetricGuard::new(AI_SEMANTIC_SEARCH_SEARCH_METRIC);
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

                let (range, snippet) =
                    snippet_range_and_text(&doc.original, &doc.normalized, &normalized_query);

                Some(SearchResult {
                    path: path.clone(),
                    range,
                    kind: "file".to_string(),
                    score,
                    snippet,
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

fn snippet_range_and_text(original: &str, normalized: &str, query: &str) -> (Range<usize>, String) {
    const MATCH_CONTEXT_BYTES: usize = 30;
    const PREFIX_FALLBACK_CHARS: usize = 80;

    if query.is_empty() {
        return (0..0, String::new());
    }

    let range = if let Some(pos) = normalized.find(query) {
        // `normalize` preserves text length (one ASCII byte per input byte) so normalized match
        // offsets are also valid byte offsets in `original` (modulo UTF-8 character boundaries).
        let start = pos.saturating_sub(MATCH_CONTEXT_BYTES);
        let end = (pos + query.len() + MATCH_CONTEXT_BYTES).min(original.len());
        let start = clamp_char_boundary_before(original, start);
        let end = clamp_char_boundary_after(original, end).max(start);
        start..end
    } else {
        // Best-effort fallback: avoid returning a whole-file range for fuzzy matches so downstream
        // callers can still reason about where the match likely occurred.
        prefix_char_range(original, PREFIX_FALLBACK_CHARS)
    };

    let snippet = original
        .get(range.clone())
        .unwrap_or_default()
        .trim()
        .to_string();

    (range, snippet)
}

fn clamp_char_boundary_before(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

fn clamp_char_boundary_after(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while offset < text.len() && !text.is_char_boundary(offset) {
        offset += 1;
    }
    offset
}

fn prefix_char_range(text: &str, max_chars: usize) -> Range<usize> {
    let end = text
        .char_indices()
        .nth(max_chars)
        .map(|(idx, _)| idx)
        .unwrap_or(text.len());
    0..end
}

#[cfg(test)]
mod metric_guard_tests {
    use super::*;

    #[test]
    fn semantic_search_metric_guard_records_panics() {
        let _guard = crate::test_support::metrics_lock()
            .lock()
            .expect("metrics lock poisoned");
        let metrics = MetricsRegistry::global();

        let before = metrics.snapshot();
        let before_requests = before
            .methods
            .get(AI_SEMANTIC_SEARCH_SEARCH_METRIC)
            .map(|m| m.request_count)
            .unwrap_or(0);
        let before_panics = before
            .methods
            .get(AI_SEMANTIC_SEARCH_SEARCH_METRIC)
            .map(|m| m.panic_count)
            .unwrap_or(0);

        let result = std::panic::catch_unwind(|| {
            let _guard = MetricGuard::new(AI_SEMANTIC_SEARCH_SEARCH_METRIC);
            panic!("boom");
        });
        assert!(result.is_err(), "expected metric guard scope to panic");

        let after = metrics.snapshot();
        let after_requests = after
            .methods
            .get(AI_SEMANTIC_SEARCH_SEARCH_METRIC)
            .map(|m| m.request_count)
            .unwrap_or(0);
        let after_panics = after
            .methods
            .get(AI_SEMANTIC_SEARCH_SEARCH_METRIC)
            .map(|m| m.panic_count)
            .unwrap_or(0);

        assert!(
            after_requests >= before_requests.saturating_add(1),
            "expected {AI_SEMANTIC_SEARCH_SEARCH_METRIC} request_count to increment"
        );
        assert!(
            after_panics >= before_panics.saturating_add(1),
            "expected {AI_SEMANTIC_SEARCH_SEARCH_METRIC} panic_count to increment"
        );
    }
}

#[cfg(feature = "embeddings")]
mod embeddings {
    use super::{SearchResult, SemanticSearch};
    use crate::audit;
    use crate::client::validate_local_only_url;
    use crate::embeddings::cache::{
        EmbeddingCacheKey as MemoryCacheKey, EmbeddingCacheKeyBuilder, EmbeddingVectorCache,
    };
    use crate::embeddings::disk_cache::{
        DiskEmbeddingCache, EmbeddingCacheKey as DiskCacheKey, DISK_CACHE_NAMESPACE_V1,
    };
    use crate::http::map_reqwest_error;
    use crate::llm_privacy::{PrivacyFilter, SanitizationSession};
    use crate::privacy::redact_file_paths;
    use crate::AiError;
    use nova_core::ProjectDatabase;
    use nova_fuzzy::{fuzzy_match, MatchKind};
    use nova_config::{AiConfig, AiProviderKind};
    use reqwest::blocking::Client as BlockingClient;
    use reqwest::StatusCode;
    use serde::{Deserialize, Serialize};
    use std::cmp::Ordering;
    use std::collections::hash_map::DefaultHasher;
    use std::collections::BTreeMap;
    use std::hash::{Hash, Hasher};
    use std::ops::Range;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU8, Ordering as AtomicOrdering};
    use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
    use std::time::Duration;
    use tracing::warn;
    use url::Url;

    use hnsw_rs::prelude::*;

    fn warn_poisoned_mutex_once() {
        static WARNED: OnceLock<()> = OnceLock::new();
        WARNED.get_or_init(|| {
            tracing::warn!(
                target = "nova.ai",
                "EmbeddingSemanticSearch index mutex was poisoned by a previous panic; attempting best-effort recovery"
            );
        });
    }

    #[cfg(feature = "embeddings-local")]
    fn warn_poisoned_local_embedder_mutex_once() {
        static WARNED: OnceLock<()> = OnceLock::new();
        WARNED.get_or_init(|| {
            tracing::warn!(
                target = "nova.ai",
                "semantic search local embedder mutex was poisoned by a previous panic; attempting best-effort recovery"
            );
        });
    }

    enum HnswRayonPool {
        /// A dedicated pool used for `hnsw_rs` rebuild/search.
        ///
        /// `hnsw_rs` uses Rayon internally; running it inside our own pool keeps parallelism
        /// bounded in resource-constrained environments (CI sandboxes, editor/LSP test harnesses)
        /// and avoids mutating Rayon's process-global pool (a surprising library side effect that
        /// can interfere with unrelated Rayon users in the host process).
        Rayon(rayon::ThreadPool),
        /// Fallback when thread creation fails.
        ///
        /// In this mode we execute the closure directly, meaning `hnsw_rs` will use Rayon's global
        /// pool. This preserves functional correctness at the cost of unbounded parallelism.
        Inline,
    }

    impl HnswRayonPool {
        fn new() -> Self {
            match rayon::ThreadPoolBuilder::new()
                // Keep CI/editor sandboxes safe by default.
                .num_threads(1)
                .thread_name(|idx| format!("nova-ai-embeddings-{idx}"))
                .build()
            {
                Ok(pool) => Self::Rayon(pool),
                Err(err) => {
                    warn!(
                        target = "nova.ai",
                        ?err,
                        "failed to create dedicated Rayon pool for embedding semantic search; falling back to Rayon's global pool"
                    );
                    Self::Inline
                }
            }
        }

        fn install<OP, R>(&self, op: OP) -> R
        where
            OP: FnOnce() -> R + Send,
            R: Send,
        {
            match self {
                Self::Rayon(pool) => pool.install(op),
                Self::Inline => op(),
            }
        }
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

        pub fn dims(&self) -> usize {
            self.dims
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

    const OLLAMA_EMBED_ENDPOINT_UNKNOWN: u8 = 0;
    const OLLAMA_EMBED_ENDPOINT_SUPPORTED: u8 = 1;
    const OLLAMA_EMBED_ENDPOINT_UNSUPPORTED: u8 = 2;

    #[derive(Clone)]
    pub(super) struct ProviderEmbedder {
        provider_kind: AiProviderKind,
        base_url: Url,
        model: String,
        api_key: Option<String>,
        azure_deployment: Option<String>,
        azure_api_version: Option<String>,
        batch_size: usize,
        ollama_embed_endpoint: Arc<AtomicU8>,
        client: BlockingClient,
        backend_id: &'static str,
        endpoint_id: String,
        memory_cache: Arc<EmbeddingVectorCache>,
        disk_cache: Option<Arc<DiskEmbeddingCache>>,
        privacy: Arc<PrivacyFilter>,
        redact_paths: bool,
    }

    impl ProviderEmbedder {
        fn try_new(
            provider_kind: AiProviderKind,
            base_url: Url,
            model: String,
            api_key: Option<String>,
            timeout: Duration,
            max_memory_bytes: usize,
            disk_cache: Option<Arc<DiskEmbeddingCache>>,
            batch_size: usize,
            privacy: Arc<PrivacyFilter>,
            redact_paths: bool,
        ) -> Result<Self, AiError> {
            let client = BlockingClient::builder().timeout(timeout).build()?;
            let backend_id = match &provider_kind {
                AiProviderKind::Ollama => "ollama",
                AiProviderKind::OpenAiCompatible => "openai_compatible",
                AiProviderKind::OpenAi => "openai",
                AiProviderKind::AzureOpenAi => "azure_open_ai",
                AiProviderKind::Http => "http",
                _ => "unknown",
            };
            let endpoint_id = match &provider_kind {
                AiProviderKind::Ollama => {
                    let base_str = base_url.as_str().trim_end_matches('/').to_string();
                    Url::parse(&format!("{base_str}/"))
                        .ok()
                        .and_then(|base| {
                            let base_path = base.path().trim_end_matches('/');
                            let join_path = if base_path.ends_with("/api") {
                                "embed"
                            } else {
                                "api/embed"
                            };
                            base.join(join_path).ok()
                        })
                        .map(|url| url.to_string())
                        .unwrap_or_else(|| base_url.to_string())
                }
                AiProviderKind::OpenAiCompatible | AiProviderKind::OpenAi | AiProviderKind::Http => {
                    openai_compatible_endpoint(&base_url, "/embeddings")
                        .map(|url| url.to_string())
                        .unwrap_or_else(|_| base_url.to_string())
                }
                _ => base_url.to_string(),
            };
            Ok(Self {
                provider_kind,
                base_url,
                model,
                api_key,
                azure_deployment: None,
                azure_api_version: None,
                batch_size: batch_size.max(1),
                ollama_embed_endpoint: Arc::new(AtomicU8::new(OLLAMA_EMBED_ENDPOINT_UNKNOWN)),
                client,
                backend_id,
                endpoint_id,
                memory_cache: Arc::new(EmbeddingVectorCache::new(max_memory_bytes)),
                disk_cache,
                privacy,
                redact_paths,
            })
        }

        fn disk_key_for(&self, input: &str) -> DiskCacheKey {
            DiskCacheKey::new(
                DISK_CACHE_NAMESPACE_V1,
                self.backend_id,
                &self.endpoint_id,
                &self.model,
                input.as_bytes(),
            )
        }

        fn memory_key_for(&self, input: &str) -> MemoryCacheKey {
            let mut builder = EmbeddingCacheKeyBuilder::new(
                "nova-ai-semantic-search-provider-embeddings-memory-cache-v1",
            );
            builder.push_str(self.backend_id);
            builder.push_str(&self.endpoint_id);
            builder.push_str(&self.model);
            builder.push_str(input);
            builder.finish()
        }

        fn sanitize_text(&self, session: &mut SanitizationSession, text: &str) -> String {
            let sanitized = if text.contains('\n') {
                // EmbeddingSemanticSearch document inputs always include path + snippet metadata
                // separated by newlines. Treat multi-line inputs as code-like documents so identifier
                // anonymization applies consistently.
                self.privacy.sanitize_code_text(session, text)
            } else {
                // Search queries are natural-language by default. Preserve their terms (avoid
                // anonymizing every word) while still applying redact_patterns and fenced-code
                // sanitization.
                self.privacy.sanitize_prompt_text(session, text)
            };
            if self.redact_paths {
                redact_file_paths(&sanitized)
            } else {
                sanitized
            }
        }

        fn sanitize_query_text(&self, session: &mut SanitizationSession, text: &str) -> String {
            let sanitized = self.privacy.sanitize_prompt_text(session, text);
            if self.redact_paths {
                redact_file_paths(&sanitized)
            } else {
                sanitized
            }
        }

        fn embed_uncached(&self, text: &str) -> Result<Vec<f32>, AiError> {
            match &self.provider_kind {
                AiProviderKind::Ollama => self.embed_ollama(text),
                AiProviderKind::OpenAiCompatible | AiProviderKind::OpenAi | AiProviderKind::Http => {
                    self.embed_openai_compatible(text)
                }
                AiProviderKind::AzureOpenAi => self.embed_azure_openai(text),
                _ => Err(AiError::InvalidConfig(format!(
                    "provider-backed embeddings are not supported for ai.provider.kind={:?}",
                    self.provider_kind
                ))),
            }
        }

        fn embed_openai_compatible(&self, text: &str) -> Result<Vec<f32>, AiError> {
            let input = [text.to_string()];
            let mut batch = self.embed_openai_compatible_batch(&input)?;
            batch.pop().ok_or_else(|| {
                AiError::UnexpectedResponse("missing embeddings data[0].embedding".into())
            })
        }

        fn embed_openai_compatible_batch(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>, AiError> {
            if inputs.is_empty() {
                return Ok(Vec::new());
            }

            let url = openai_compatible_endpoint(&self.base_url, "/embeddings")?;
            let body = OpenAiEmbeddingBatchRequest {
                model: &self.model,
                input: inputs,
            };

            let mut request = self.client.post(url).json(&body);
            if let Some(key) = self.api_key.as_deref() {
                request = request.bearer_auth(key);
            }

            let response = request
                .send()
                .map_err(map_reqwest_error)?
                .error_for_status()
                .map_err(map_reqwest_error)?;
            let parsed: OpenAiEmbeddingResponse = response.json().map_err(map_reqwest_error)?;
            parse_openai_embeddings(parsed, inputs.len())
        }

        fn embed_azure_openai_batch(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>, AiError> {
            if inputs.is_empty() {
                return Ok(Vec::new());
            }

            let api_key = self.api_key.as_deref().ok_or_else(|| {
                AiError::InvalidConfig("Azure OpenAI embeddings require ai.api_key".into())
            })?;
            let deployment = self.azure_deployment.as_deref().ok_or_else(|| {
                AiError::InvalidConfig(
                    "Azure OpenAI embeddings require ai.provider.azure_deployment".into(),
                )
            })?;
            let api_version = self
                .azure_api_version
                .as_deref()
                .unwrap_or("2024-02-01");

            let url = azure_openai_embeddings_endpoint(&self.base_url, deployment, api_version)?;
            let body = AzureOpenAiEmbeddingBatchRequest { input: inputs };

            let response = self
                .client
                .post(url)
                .header("api-key", api_key)
                .json(&body)
                .send()
                .map_err(map_reqwest_error)?
                .error_for_status()
                .map_err(map_reqwest_error)?;

            let parsed: OpenAiEmbeddingResponse = response.json().map_err(map_reqwest_error)?;
            parse_openai_embeddings(parsed, inputs.len())
        }

        fn embed_azure_openai(&self, text: &str) -> Result<Vec<f32>, AiError> {
            let mut batch = self.embed_azure_openai_batch(&[text.to_string()])?;
            batch.pop().ok_or_else(|| {
                AiError::UnexpectedResponse("missing embeddings data[0].embedding".into())
            })
        }

        fn embed_ollama(&self, text: &str) -> Result<Vec<f32>, AiError> {
            let input = text.to_string();
            self.embed_ollama_batch(std::slice::from_ref(&input))?
                .into_iter()
                .next()
                .ok_or_else(|| {
                    AiError::UnexpectedResponse(
                        "missing embedding output from Ollama provider embedder".into(),
                    )
                })
        }

        fn ollama_endpoint(&self, path: &str) -> Result<Url, AiError> {
            let base_str = self.base_url.as_str().trim_end_matches('/').to_string();
            let base = Url::parse(&format!("{base_str}/"))?;
            let base_path = base.path().trim_end_matches('/');
            let mut relative = path.trim_start_matches('/');
            if base_path.ends_with("/api") && relative.starts_with("api/") {
                relative = relative.trim_start_matches("api/");
            }
            Ok(base.join(relative)?)
        }

        fn embed_ollama_via_embed_endpoint(
            &self,
            input: &[String],
        ) -> Result<Option<Vec<Vec<f32>>>, AiError> {
            let url = self.ollama_endpoint("/api/embed")?;
            let body = OllamaEmbedRequest {
                model: &self.model,
                input,
            };

            let response = self
                .client
                .post(url)
                .json(&body)
                .send()
                .map_err(map_reqwest_error)?;
            if response.status() == StatusCode::NOT_FOUND {
                return Ok(None);
            }

            let response = response
                .error_for_status()
                .map_err(map_reqwest_error)?;
            let parsed: OllamaEmbedResponse = response.json().map_err(map_reqwest_error)?;
            let embeddings = parsed.into_embeddings().ok_or_else(|| {
                AiError::UnexpectedResponse(
                    "missing `embeddings` in Ollama /api/embed response".into(),
                )
            })?;

            if embeddings.len() != input.len() {
                return Err(AiError::UnexpectedResponse(format!(
                    "Ollama /api/embed returned {} embeddings for {} inputs",
                    embeddings.len(),
                    input.len()
                )));
            }
            if embeddings.iter().any(|embedding| embedding.is_empty()) {
                return Err(AiError::UnexpectedResponse(
                    "Ollama /api/embed returned empty embedding vector".into(),
                ));
            }

            Ok(Some(embeddings))
        }

        fn embed_ollama_via_legacy_endpoint(&self, input: &[String]) -> Result<Vec<Vec<f32>>, AiError> {
            if input.is_empty() {
                return Ok(Vec::new());
            }

            let url = self.ollama_endpoint("/api/embeddings")?;
            let mut out = Vec::with_capacity(input.len());

            for prompt in input {
                let body = OllamaEmbeddingRequest {
                    model: &self.model,
                    prompt,
                };

                let response = self
                    .client
                    .post(url.clone())
                    .json(&body)
                    .send()
                    .map_err(map_reqwest_error)?
                    .error_for_status()
                    .map_err(map_reqwest_error)?;
                let parsed: OllamaEmbeddingResponse = response.json().map_err(map_reqwest_error)?;

                if parsed.embedding.is_empty() {
                    return Err(AiError::UnexpectedResponse(
                        "missing `embedding` in Ollama /api/embeddings response".into(),
                    ));
                }
                out.push(parsed.embedding);
            }

            Ok(out)
        }

        fn embed_ollama_batch(&self, input: &[String]) -> Result<Vec<Vec<f32>>, AiError> {
            if input.is_empty() {
                return Ok(Vec::new());
            }

            let mut out = Vec::with_capacity(input.len());

            for chunk in input.chunks(self.batch_size) {
                let mode = self.ollama_embed_endpoint.load(AtomicOrdering::Acquire);
                if mode != OLLAMA_EMBED_ENDPOINT_UNSUPPORTED {
                    match self.embed_ollama_via_embed_endpoint(chunk) {
                        Ok(Some(embeddings)) => {
                            self.ollama_embed_endpoint.store(
                                OLLAMA_EMBED_ENDPOINT_SUPPORTED,
                                AtomicOrdering::Release,
                            );
                            out.extend(embeddings);
                            continue;
                        }
                        Ok(None) => {
                            self.ollama_embed_endpoint.store(
                                OLLAMA_EMBED_ENDPOINT_UNSUPPORTED,
                                AtomicOrdering::Release,
                            );
                        }
                        Err(err) => {
                            warn!(
                                target = "nova.ai",
                                ?err,
                                "Ollama /api/embed failed; falling back to /api/embeddings"
                            );
                        }
                    }
                }

                out.extend(self.embed_ollama_via_legacy_endpoint(chunk)?);
            }

            Ok(out)
        }
    }

    impl Embedder for ProviderEmbedder {
        fn embed(&self, text: &str) -> Result<Vec<f32>, AiError> {
            let mut session = self.privacy.new_session();
            let sanitized = self.sanitize_query_text(&mut session, text);

            let memory_key = self.memory_key_for(&sanitized);
            if let Some(hit) = self.memory_cache.get(memory_key) {
                return Ok(hit);
            }

            let disk_key = self.disk_key_for(&sanitized);
            if let Some(disk) = self.disk_cache.as_ref() {
                if let Ok(Some(hit)) = disk.load(disk_key) {
                    self.memory_cache.insert(memory_key, hit.clone());
                    return Ok(hit);
                }
            }

            let embedding = self.embed_uncached(&sanitized)?;

            if !embedding.is_empty() {
                self.memory_cache.insert(memory_key, embedding.clone());
                if let Some(disk) = self.disk_cache.as_ref() {
                    let _ = disk.store(disk_key, &embedding);
                }
            }

            Ok(embedding)
        }

        fn embed_batch(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>, AiError> {
            if inputs.is_empty() {
                return Ok(Vec::new());
            }

            // Always sanitize inputs in a single session so anonymization stays stable across the
            // batch even when some embeddings are served from cache.
            let mut session = self.privacy.new_session();
            let sanitized = inputs
                .iter()
                .map(|text| self.sanitize_text(&mut session, text))
                .collect::<Vec<_>>();

            let mut out = vec![None::<Vec<f32>>; inputs.len()];
            let mut miss_indices = Vec::new();
            let mut miss_memory_keys = Vec::new();
            let mut miss_disk_keys = Vec::new();
            let mut miss_inputs = Vec::new();

            for (idx, text) in sanitized.iter().enumerate() {
                let memory_key = self.memory_key_for(text);
                if let Some(hit) = self.memory_cache.get(memory_key) {
                    out[idx] = Some(hit);
                } else {
                    miss_indices.push(idx);
                    miss_memory_keys.push(memory_key);
                    miss_disk_keys.push(self.disk_key_for(text));
                    miss_inputs.push(text.clone());
                }
            }

            if let Some(disk) = self.disk_cache.as_ref() {
                let mut still_indices = Vec::new();
                let mut still_memory_keys = Vec::new();
                let mut still_disk_keys = Vec::new();
                let mut still_inputs = Vec::new();

                for (((orig_idx, memory_key), disk_key), text) in miss_indices
                    .into_iter()
                    .zip(miss_memory_keys.into_iter())
                    .zip(miss_disk_keys.into_iter())
                    .zip(miss_inputs.into_iter())
                {
                    if let Ok(Some(hit)) = disk.load(disk_key) {
                        out[orig_idx] = Some(hit.clone());
                        self.memory_cache.insert(memory_key, hit);
                    } else {
                        still_indices.push(orig_idx);
                        still_memory_keys.push(memory_key);
                        still_disk_keys.push(disk_key);
                        still_inputs.push(text);
                    }
                }

                miss_indices = still_indices;
                miss_memory_keys = still_memory_keys;
                miss_disk_keys = still_disk_keys;
                miss_inputs = still_inputs;
            }

            if !miss_inputs.is_empty() {
                let batch_size = self.batch_size.max(1);
                let embeddings = match &self.provider_kind {
                    AiProviderKind::Ollama => self.embed_ollama_batch(&miss_inputs)?,
                    AiProviderKind::OpenAiCompatible | AiProviderKind::OpenAi | AiProviderKind::Http => {
                        let mut out = Vec::with_capacity(miss_inputs.len());
                        for chunk in miss_inputs.chunks(batch_size) {
                            out.extend(self.embed_openai_compatible_batch(chunk)?);
                        }
                        out
                    }
                    AiProviderKind::AzureOpenAi => {
                        let mut out = Vec::with_capacity(miss_inputs.len());
                        for chunk in miss_inputs.chunks(batch_size) {
                            out.extend(self.embed_azure_openai_batch(chunk)?);
                        }
                        out
                    }
                    _ => {
                        return Err(AiError::InvalidConfig(format!(
                            "provider-backed embeddings are not supported for ai.provider.kind={:?}",
                            self.provider_kind
                        )));
                    }
                };

                if embeddings.len() != miss_inputs.len() {
                    return Err(AiError::UnexpectedResponse(format!(
                        "embedder returned unexpected batch size: expected {}, got {}",
                        miss_inputs.len(),
                        embeddings.len()
                    )));
                }

                for (((orig_idx, memory_key), disk_key), embedding) in miss_indices
                    .into_iter()
                    .zip(miss_memory_keys.into_iter())
                    .zip(miss_disk_keys.into_iter())
                    .zip(embeddings.into_iter())
                {
                    out[orig_idx] = Some(embedding.clone());

                    if !embedding.is_empty() {
                        self.memory_cache.insert(memory_key, embedding.clone());
                        if let Some(disk) = self.disk_cache.as_ref() {
                            let _ = disk.store(disk_key, &embedding);
                        }
                    }
                }
            }

            out.into_iter()
                .enumerate()
                .map(|(idx, item)| {
                    item.ok_or_else(|| {
                        AiError::UnexpectedResponse(format!("missing embeddings data for index {idx}"))
                    })
                })
                .collect()
        }
    }

    pub(super) fn provider_embedder_from_config(config: &AiConfig) -> Option<ProviderEmbedder> {
        let provider_kind = config.provider.kind.clone();

        if config.privacy.local_only {
            match &provider_kind {
                AiProviderKind::Ollama | AiProviderKind::OpenAiCompatible | AiProviderKind::Http => {
                    if let Err(err) = validate_local_only_url(&config.provider.url) {
                        warn!(
                            target = "nova.ai",
                            provider = ?provider_kind,
                            url = %audit::sanitize_url_for_log(&config.provider.url),
                            "ai.privacy.local_only=true forbids provider-backed embeddings to non-loopback urls ({err}); falling back to hash embeddings"
                        );
                        return None;
                    }
                }
                AiProviderKind::OpenAi
                | AiProviderKind::Anthropic
                | AiProviderKind::Gemini
                | AiProviderKind::AzureOpenAi => {
                    warn!(
                        target = "nova.ai",
                        provider = ?provider_kind,
                        "ai.privacy.local_only=true forbids provider-backed embeddings for cloud providers; falling back to hash embeddings"
                    );
                    return None;
                }
                _ => {
                    warn!(
                        target = "nova.ai",
                        provider = ?provider_kind,
                        "ai.privacy.local_only=true forbids provider-backed embeddings for this provider; falling back to hash embeddings"
                    );
                    return None;
                }
            }
        }

        let model = config
            .embeddings
            .model
            .clone()
            .unwrap_or_else(|| config.provider.model.clone());
        let batch_size = config.embeddings.batch_size;
        let timeout = config
            .embeddings
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or_else(|| config.provider.timeout());
        let max_memory_bytes = (config.embeddings.max_memory_bytes.0).min(usize::MAX as u64) as usize;

        let api_key = config
            .api_key
            .clone()
            .filter(|key| !key.trim().is_empty());
        let disk_cache = DiskEmbeddingCache::new(config.embeddings.model_dir.clone())
            .map(Arc::new)
            .ok();
        let redact_paths = !config.privacy.local_only && !config.privacy.include_file_paths;
        let privacy = match PrivacyFilter::new(&config.privacy) {
            Ok(filter) => Arc::new(filter),
            Err(err) => {
                warn!(
                    target = "nova.ai",
                    provider = ?config.provider.kind,
                    ?err,
                    "failed to build embeddings privacy filter; falling back to hash embeddings"
                );
                return None;
            }
        };

        match provider_kind {
            AiProviderKind::Ollama | AiProviderKind::OpenAiCompatible | AiProviderKind::Http => {
                match ProviderEmbedder::try_new(
                    provider_kind,
                    config.provider.url.clone(),
                    model,
                    api_key,
                    timeout,
                    max_memory_bytes,
                    disk_cache.clone(),
                    batch_size,
                    privacy.clone(),
                    redact_paths,
                ) {
                    Ok(embedder) => Some(embedder),
                    Err(err) => {
                        warn!(
                            target = "nova.ai",
                            provider = ?config.provider.kind,
                            ?err,
                            "failed to build provider embedder; falling back to hash embeddings"
                        );
                        None
                    }
                }
            }
            AiProviderKind::AzureOpenAi => {
                let Some(api_key) = api_key else {
                    warn!(
                        target = "nova.ai",
                        "ai.embeddings.backend=provider with ai.provider.kind=azure_open_ai requires ai.api_key; falling back to hash embeddings"
                    );
                    return None;
                };

                let Some(deployment) = config
                    .embeddings
                    .model
                    .clone()
                    .or_else(|| config.provider.azure_deployment.clone())
                else {
                    warn!(
                        target = "nova.ai",
                        "ai.embeddings.backend=provider with ai.provider.kind=azure_open_ai requires ai.embeddings.model or ai.provider.azure_deployment; falling back to hash embeddings"
                    );
                    return None;
                };
                if deployment.trim().is_empty() {
                    warn!(
                        target = "nova.ai",
                        "ai.embeddings.backend=provider with ai.provider.kind=azure_open_ai requires a non-empty deployment; falling back to hash embeddings"
                    );
                    return None;
                }

                let api_version = config
                    .provider
                    .azure_api_version
                    .clone()
                    .filter(|v| !v.trim().is_empty())
                    .unwrap_or_else(|| "2024-02-01".to_string());

                match ProviderEmbedder::try_new(
                    provider_kind,
                    config.provider.url.clone(),
                    // Azure selects the embedding model by deployment, so use deployment as the
                    // cache model identifier.
                    deployment.clone(),
                    Some(api_key),
                    timeout,
                    max_memory_bytes,
                    disk_cache.clone(),
                    batch_size,
                    privacy.clone(),
                    redact_paths,
                ) {
                    Ok(mut embedder) => {
                        embedder.azure_deployment = Some(deployment);
                        embedder.azure_api_version = Some(api_version.clone());
                        embedder.endpoint_id = azure_openai_embeddings_endpoint(
                            &embedder.base_url,
                            embedder.azure_deployment.as_deref().unwrap_or_default(),
                            &api_version,
                        )
                        .map(|url| url.to_string())
                        .unwrap_or_else(|_| embedder.base_url.to_string());
                        Some(embedder)
                    }
                    Err(err) => {
                        warn!(
                            target = "nova.ai",
                            provider = ?config.provider.kind,
                            ?err,
                            "failed to build provider embedder; falling back to hash embeddings"
                        );
                        None
                    }
                }
            }
            AiProviderKind::OpenAi => {
                let Some(api_key) = api_key else {
                    warn!(
                        target = "nova.ai",
                        "ai.embeddings.backend=provider with ai.provider.kind=open_ai requires ai.api_key; falling back to hash embeddings"
                    );
                    return None;
                };

                match ProviderEmbedder::try_new(
                    provider_kind,
                    config.provider.url.clone(),
                    model,
                    Some(api_key),
                    timeout,
                    max_memory_bytes,
                    disk_cache,
                    batch_size,
                    privacy.clone(),
                    redact_paths,
                ) {
                    Ok(embedder) => Some(embedder),
                    Err(err) => {
                        warn!(
                            target = "nova.ai",
                            provider = ?config.provider.kind,
                            ?err,
                            "failed to build provider embedder; falling back to hash embeddings"
                        );
                        None
                    }
                }
            }
            other => {
                warn!(
                    target = "nova.ai",
                    provider = ?other,
                    "ai.embeddings.backend=provider is not supported for this ai.provider.kind; falling back to hash embeddings"
                );
                None
            }
        }
    }

    fn openai_compatible_endpoint(base_url: &Url, path: &str) -> Result<Url, AiError> {
        // Accept both:
        // - http://localhost:8000  (we will append /v1/...)
        // - http://localhost:8000/v1  (we will append /...)
        let mut base = base_url.clone();
        let base_str = base.as_str().trim_end_matches('/').to_string();
        base = Url::parse(&format!("{base_str}/"))?;

        let base_path = base.path().trim_end_matches('/');
        if base_path.ends_with("/v1") {
            Ok(base.join(path.trim_start_matches('/'))?)
        } else {
            Ok(base.join(&format!("v1/{}", path.trim_start_matches('/')))?)
        }
    }

    fn azure_openai_embeddings_endpoint(
        endpoint: &Url,
        deployment: &str,
        api_version: &str,
    ) -> Result<Url, AiError> {
        let mut base = endpoint.clone();
        let base_str = base.as_str().trim_end_matches('/').to_string();
        base = Url::parse(&format!("{base_str}/"))?;
        let base_path = base.path().trim_end_matches('/');

        let join_path = if base_path.ends_with("/openai") {
            format!("deployments/{deployment}/embeddings")
        } else {
            format!("openai/deployments/{deployment}/embeddings")
        };

        let mut url = base.join(&join_path)?;
        url.query_pairs_mut()
            .append_pair("api-version", api_version);
        Ok(url)
    }

    #[derive(Serialize)]
    struct OpenAiEmbeddingBatchRequest<'a> {
        model: &'a str,
        input: &'a [String],
    }

    #[derive(Serialize)]
    struct AzureOpenAiEmbeddingBatchRequest<'a> {
        input: &'a [String],
    }

    #[derive(Deserialize)]
    struct OpenAiEmbeddingResponse {
        #[serde(default)]
        data: Vec<OpenAiEmbeddingData>,
    }

    #[derive(Deserialize)]
    struct OpenAiEmbeddingData {
        #[serde(default)]
        embedding: Vec<f32>,
        #[serde(default)]
        index: Option<usize>,
    }

    fn parse_openai_embeddings(
        response: OpenAiEmbeddingResponse,
        expected: usize,
    ) -> Result<Vec<Vec<f32>>, AiError> {
        let mut out = vec![None::<Vec<f32>>; expected];
        for (pos, item) in response.data.into_iter().enumerate() {
            let idx = item.index.unwrap_or(pos);
            if idx >= expected {
                return Err(AiError::UnexpectedResponse(format!(
                    "embeddings index {} out of range (expected < {expected})",
                    idx
                )));
            }
            if out[idx].is_some() {
                return Err(AiError::UnexpectedResponse(format!(
                    "duplicate embeddings index {}",
                    idx
                )));
            }
            out[idx] = Some(item.embedding);
        }

        let mut dims: Option<usize> = None;
        let mut embeddings = Vec::with_capacity(expected);

        for (idx, item) in out.into_iter().enumerate() {
            let embedding = item.filter(|v| !v.is_empty()).ok_or_else(|| {
                AiError::UnexpectedResponse(format!("missing embeddings data for index {idx}"))
            })?;

            match dims {
                None => dims = Some(embedding.len()),
                Some(expected_dims) if embedding.len() != expected_dims => {
                    return Err(AiError::UnexpectedResponse(format!(
                        "inconsistent embedding dimensions: expected {expected_dims}, got {} for index {idx}",
                        embedding.len()
                    )));
                }
                _ => {}
            }

            embeddings.push(embedding);
        }

        Ok(embeddings)
    }

    #[derive(Serialize)]
    struct OllamaEmbeddingRequest<'a> {
        model: &'a str,
        prompt: &'a str,
    }

    #[derive(Deserialize)]
    struct OllamaEmbeddingResponse {
        #[serde(default)]
        embedding: Vec<f32>,
    }

    #[derive(Serialize)]
    struct OllamaEmbedRequest<'a> {
        model: &'a str,
        input: &'a [String],
    }

    #[derive(Deserialize)]
    struct OllamaEmbedResponse {
        #[serde(default)]
        embeddings: Vec<Vec<f32>>,
        #[serde(default)]
        embedding: Vec<f32>,
    }

    impl OllamaEmbedResponse {
        fn into_embeddings(self) -> Option<Vec<Vec<f32>>> {
            if !self.embeddings.is_empty() {
                Some(self.embeddings)
            } else if !self.embedding.is_empty() {
                Some(vec![self.embedding])
            } else {
                None
            }
        }
    }

    #[cfg(feature = "embeddings-local")]
    pub struct LocalEmbedder {
        batch_size: usize,
        embedder: Mutex<fastembed::TextEmbedding>,
        cache: EmbeddingVectorCache,
        model_id: String,
        model_dir: PathBuf,
    }

    #[cfg(feature = "embeddings-local")]
    impl std::fmt::Debug for LocalEmbedder {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("LocalEmbedder")
                .field("model_id", &self.model_id)
                .field("model_dir", &self.model_dir)
                .field("batch_size", &self.batch_size)
                .finish()
        }
    }

    #[cfg(feature = "embeddings-local")]
    impl LocalEmbedder {
        pub fn from_config(config: &nova_config::AiEmbeddingsConfig) -> Result<Self, crate::AiError> {
            use crate::AiError;

            let model_id = config.local_model.trim();
            if model_id.is_empty() {
                return Err(AiError::InvalidConfig(
                    "ai.embeddings.local_model must be non-empty when backend=\"local\"".to_string(),
                ));
            }

            if config.model_dir.as_os_str().is_empty() {
                return Err(AiError::InvalidConfig(
                    "ai.embeddings.model_dir must be non-empty when embeddings are enabled"
                        .to_string(),
                ));
            }

            let model_dir = config.model_dir.clone();
            std::fs::create_dir_all(&model_dir).map_err(|source| {
                AiError::InvalidConfig(format!(
                    "failed to create ai.embeddings.model_dir {}: {source}",
                    model_dir.display()
                ))
            })?;

            let max_memory_bytes = (config.max_memory_bytes.0).min(usize::MAX as u64) as usize;

            let model = fastembed_model_from_id(model_id).map_err(|err| {
                AiError::InvalidConfig(format!(
                    "unsupported ai.embeddings.local_model={model_id:?}: {err}"
                ))
            })?;

            let options = fastembed::InitOptions::new(model)
                .with_cache_dir(model_dir.clone())
                .with_show_download_progress(false);

            let embedder = fastembed::TextEmbedding::try_new(options).map_err(|source| {
                AiError::InvalidConfig(format!(
                    "failed to initialize local embedding model {model_id:?} (cache dir {}): {source}",
                    model_dir.display()
                ))
            })?;

            Ok(Self {
                batch_size: config.batch_size.max(1),
                embedder: Mutex::new(embedder),
                cache: EmbeddingVectorCache::new(max_memory_bytes),
                model_id: model_id.to_string(),
                model_dir,
            })
        }
    }

    #[cfg(feature = "embeddings-local")]
    fn fastembed_model_from_id(id: &str) -> Result<fastembed::EmbeddingModel, String> {
        // `fastembed` supports a fixed set of model IDs; map a few common aliases and
        // delegate the rest to its parser (when available).
        let trimmed = id.trim();
        if trimmed.is_empty() {
            return Err("model id is empty".to_string());
        }

        // Fast-path common aliases so users can copy/paste from HuggingFace model
        // cards without needing exact casing.
        let normalized = trimmed.to_ascii_lowercase();
        let normalized = normalized.as_str();

        let canonical = match normalized {
            "all-minilm-l6-v2" | "all_minilm_l6_v2" | "allminilm-l6-v2" => "all-MiniLM-L6-v2",
            "bge-small-en-v1.5" | "bge_small_en_v1.5" | "bge-small-en" => "bge-small-en-v1.5",
            _ => trimmed,
        };

        canonical
            .parse::<fastembed::EmbeddingModel>()
            .map_err(|err| err.to_string())
    }

    #[cfg(feature = "embeddings-local")]
    impl Embedder for LocalEmbedder {
        fn embed(&self, text: &str) -> Result<Vec<f32>, AiError> {
            let mut batch = self.embed_batch(&[text.to_string()])?;
            Ok(batch.pop().unwrap_or_default())
        }

        fn embed_batch(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>, AiError> {
            if inputs.is_empty() {
                return Ok(Vec::new());
            }

            let mut out = vec![None::<Vec<f32>>; inputs.len()];
            let mut miss_indices = Vec::new();
            let mut miss_keys = Vec::new();
            let mut miss_inputs = Vec::new();

            for (idx, text) in inputs.iter().enumerate() {
                let key = MemoryCacheKey::new("fastembed", &self.model_id, text);
                if let Some(hit) = self.cache.get(key) {
                    out[idx] = Some(hit);
                } else {
                    miss_indices.push(idx);
                    miss_keys.push(key);
                    miss_inputs.push(text.clone());
                }
            }

            if !miss_inputs.is_empty() {
                let mut embedded = Vec::with_capacity(miss_inputs.len());
                let mut embedder = self.embedder.lock().unwrap_or_else(|err| err.into_inner());

                if self.embedder.is_poisoned() {
                    warn_poisoned_local_embedder_mutex_once();
                    self.embedder.clear_poison();
                }

                for chunk in miss_inputs.chunks(self.batch_size.max(1)) {
                    let embeddings = embedder
                        .embed(chunk.to_vec(), Some(self.batch_size))
                        .map_err(|err| {
                            AiError::UnexpectedResponse(format!(
                                "fastembed embedding failed for model {}: {err}",
                                self.model_id
                            ))
                        })?;

                    if embeddings.len() != chunk.len() {
                        return Err(AiError::UnexpectedResponse(format!(
                            "fastembed returned {} embeddings for {} inputs (model {})",
                            embeddings.len(),
                            chunk.len(),
                            self.model_id
                        )));
                    }

                    embedded.extend(embeddings);
                }

                if embedded.len() != miss_inputs.len() {
                    return Err(AiError::UnexpectedResponse(format!(
                        "fastembed returned {} embeddings for {} inputs (model {})",
                        embedded.len(),
                        miss_inputs.len(),
                        self.model_id
                    )));
                }

                for ((orig_idx, key), embedding) in miss_indices
                    .into_iter()
                    .zip(miss_keys.into_iter())
                    .zip(embedded.into_iter())
                {
                    out[orig_idx] = Some(embedding.clone());
                    if !embedding.is_empty() {
                        self.cache.insert(key, embedding);
                    }
                }
            }

            out.into_iter()
                .enumerate()
                .map(|(idx, item)| {
                    item.ok_or_else(|| {
                        AiError::UnexpectedResponse(format!("missing embedding output for index {idx}"))
                    })
                })
                .collect()
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
        max_elements: usize,
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
                max_elements: 0,
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
        hnsw_pool: HnswRayonPool,
        ef_search: usize,
        max_memory_bytes: Option<usize>,
        embedding_bytes_used: usize,
        truncation_warned: bool,
    }

    impl<E: Embedder> EmbeddingSemanticSearch<E> {
        pub fn new(embedder: E) -> Self {
            Self {
                embedder,
                docs_by_path: BTreeMap::new(),
                index: Mutex::new(EmbeddedIndex::empty()),
                hnsw_pool: HnswRayonPool::new(),
                ef_search: 64,
                max_memory_bytes: None,
                embedding_bytes_used: 0,
                truncation_warned: false,
            }
        }

        fn lock_index(&self) -> MutexGuard<'_, EmbeddedIndex> {
            let mut guard = self.index.lock().unwrap_or_else(|err| err.into_inner());

            if self.index.is_poisoned() {
                warn_poisoned_mutex_once();

                // A poisoned mutex indicates a panic while the index was mid-mutation.
                // Drop potentially-inconsistent state and force a rebuild on the next search.
                let old_hnsw = guard.hnsw.take();
                guard.dims = 0;
                guard.id_to_doc.clear();
                guard.max_elements = 0;
                guard.dirty = true;

                self.index.clear_poison();

                // Dropping `hnsw_rs` structures can trigger Rayon parallel iterators. Ensure we run
                // the destructor inside our dedicated pool so we don't accidentally initialize/use
                // the process-global Rayon pool.
                self.hnsw_pool.install(|| drop(old_hnsw));
            }

            guard
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
            // Dropping `hnsw_rs` structures can trigger Rayon parallel iterators. Ensure we run the
            // destructor inside our dedicated pool so we don't accidentally initialize/use the
            // process-global Rayon pool (which may try to spawn many threads and fail in
            // constrained environments).
            let old_hnsw = {
                let mut index = self.lock_index();
                let old_hnsw = index.hnsw.take();
                index.dims = 0;
                index.id_to_doc.clear();
                index.max_elements = 0;
                index.dirty = true;
                old_hnsw
            };
            self.hnsw_pool.install(|| drop(old_hnsw));
        }

        fn rebuild_index_locked(&self, index: &mut EmbeddedIndex) {
            let index: &mut EmbeddedIndex = index;
            self.hnsw_pool.install(|| {
                #[cfg(any(test, debug_assertions))]
                {
                    index.rebuild_count = index.rebuild_count.saturating_add(1);
                }

                index.hnsw = None;
                index.dims = 0;
                index.id_to_doc.clear();
                index.max_elements = 0;

                // Pre-pass: determine target dimensionality (first non-empty embedding) and count how
                // many documents will actually be inserted. HNSW pre-allocates internal buffers based
                // on `max_elements`, so using the raw extracted-doc count can over-allocate when some
                // embeddings are empty or dimension-mismatched.
                let mut dims = 0usize;
                let mut insert_count = 0usize;
                for docs in self.docs_by_path.values() {
                    for doc in docs {
                        if doc.embedding.is_empty() {
                            continue;
                        }
                        if dims == 0 {
                            dims = doc.embedding.len();
                        }
                        if doc.embedding.len() == dims {
                            insert_count += 1;
                        }
                    }
                }

                if dims == 0 || insert_count == 0 {
                    index.dirty = false;
                    return;
                }

                index.max_elements = insert_count;
                let mut hnsw = Hnsw::new(
                    /*max_nb_connection=*/ 16,
                    /*max_elements=*/ insert_count,
                    /*nb_layer=*/ 16,
                    /*ef_construction=*/ 200,
                    DistCosine {},
                );

                let mut next_id = 0usize;
                index.id_to_doc.reserve(insert_count);
                for (path, docs) in &self.docs_by_path {
                    for (local_idx, doc) in docs.iter().enumerate() {
                        if doc.embedding.is_empty() {
                            continue;
                        }

                        if doc.embedding.len() != dims || dims == 0 {
                            continue;
                        }

                        hnsw.insert((&doc.embedding, next_id));
                        index.id_to_doc.push((path.clone(), local_idx));
                        next_id += 1;
                    }
                }

                debug_assert_eq!(next_id, insert_count);
                debug_assert_eq!(index.id_to_doc.len(), insert_count);

                hnsw.set_searching_mode(true);

                index.hnsw = Some(hnsw);
                index.dims = dims;
                index.dirty = false;
            });
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
                extract_java_symbols(path, text)
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
            // See `invalidate_index` for why we drop the old HNSW index inside our dedicated pool.
            let old_hnsw = {
                let mut index = self.lock_index();
                let old_hnsw = index.hnsw.take();
                *index = EmbeddedIndex::empty();
                old_hnsw
            };
            self.hnsw_pool.install(|| drop(old_hnsw));
        }

        fn index_project(&mut self, db: &dyn ProjectDatabase) {
            let _guard =
                super::MetricGuard::new(super::AI_SEMANTIC_SEARCH_INDEX_PROJECT_METRIC);
            // Bulk indexing is often triggered during project open/refresh. Pre-build the HNSW
            // structure here so the first `search()` call doesn't pay the full rebuild cost.
            self.clear();
            for path in db.project_files() {
                let Some(text) = db.file_text(&path) else {
                    continue;
                };
                self.index_file(path, text);
            }
            self.finalize_indexing();
        }

        fn index_file(&mut self, path: PathBuf, text: String) {
            let _guard =
                super::MetricGuard::new(super::AI_SEMANTIC_SEARCH_INDEX_FILE_METRIC);
            let mut changed = false;

            if let Some(existing) = self.docs_by_path.remove(&path) {
                let removed_bytes = Self::embedding_bytes_for_docs(&existing);
                self.embedding_bytes_used = self.embedding_bytes_used.saturating_sub(removed_bytes);
                changed = true;
            }

            if text.is_empty() {
                if changed {
                    self.invalidate_index();
                }
                return;
            }
            // If we're already at (or extremely close to) our embedding memory budget, avoid doing
            // any embedding work for this file. This keeps workspace indexing usable when
            // provider-backed embeddings are enabled (network calls are expensive) and prevents
            // doing work that will be immediately discarded.
            let remaining_budget = self.max_memory_bytes.map(|limit| {
                self.enforce_memory_budget();
                limit.saturating_sub(self.embedding_bytes_used)
            });
            if let Some(remaining) = remaining_budget {
                // Any stored embedding uses at least one `f32` (4 bytes). If we can't store that,
                // skip embedding entirely.
                if remaining < std::mem::size_of::<f32>() {
                    self.warn_truncation_once();
                    if changed {
                        self.invalidate_index();
                    }
                    return;
                }
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
            let embeddings = self.embedder.embed_batch(&inputs);

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
                        super::MetricsRegistry::global()
                            .record_error(super::AI_SEMANTIC_SEARCH_INDEX_FILE_METRIC);
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
                    super::record_semantic_search_failure(
                        super::AI_SEMANTIC_SEARCH_INDEX_FILE_METRIC,
                        &err,
                    );

                    // Best-effort fallback: attempt to embed each doc individually so partial
                    // failures don't wipe out the entire file.
                    let mut out = Vec::with_capacity(inputs.len());
                    for input in &inputs {
                        match self.embedder.embed_batch(std::slice::from_ref(input)) {
                            Ok(mut batch) if batch.len() == 1 => out.push(batch.pop()),
                            Ok(_) => out.push(None),
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
                let remaining =
                    remaining_budget.unwrap_or_else(|| limit.saturating_sub(self.embedding_bytes_used));
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

        fn finalize_indexing(&self) {
            let _guard =
                super::MetricGuard::new(super::AI_SEMANTIC_SEARCH_FINALIZE_INDEXING_METRIC);
            let mut index = self.lock_index();
            if index.dirty {
                self.rebuild_index_locked(&mut index);
            }
        }

        fn search(&self, query: &str) -> Vec<SearchResult> {
            let _guard = super::MetricGuard::new(super::AI_SEMANTIC_SEARCH_SEARCH_METRIC);
            let mut query_embedding = match self.embedder.embed(query) {
                Ok(embedding) => embedding,
                Err(err) => {
                    tracing::warn!(
                        target = "nova.ai",
                        ?err,
                        "failed to embed query; returning empty results"
                    );
                    super::record_semantic_search_failure(
                        super::AI_SEMANTIC_SEARCH_SEARCH_METRIC,
                        &err,
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

            let mut index = self.lock_index();
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

            let neighbours = self
                .hnsw_pool
                .install(|| hnsw.search(&query_embedding, 50, self.ef_search));
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

    impl<E: Embedder> Drop for EmbeddingSemanticSearch<E> {
        fn drop(&mut self) {
            // `hnsw_rs` uses Rayon internally, including in some destructors. Make sure any
            // remaining HNSW state is dropped inside our dedicated pool so we don't accidentally
            // initialize or depend on the process-global Rayon pool.
            let old_hnsw = {
                let mut index = self.lock_index();
                index.hnsw.take()
            };

            self.hnsw_pool.install(|| drop(old_hnsw));
        }
    }

    // -----------------------------------------------------------------------------
    // Test / debug-only helpers
    // -----------------------------------------------------------------------------

    #[cfg(any(test, debug_assertions))]
    impl<E: Embedder> EmbeddingSemanticSearch<E> {
        #[doc(hidden)]
        pub fn __poison_index_mutex_for_test(&self) {
            // This is used by regression tests to ensure we recover from poisoning.
            // Panicking while holding the lock will poison it.
            let mut guard = self.lock_index();

            // Make the internal index state inconsistent so recovery has to do real work.
            let old_hnsw = guard.hnsw.take();
            guard.dims = 0;
            guard.id_to_doc.clear();
            guard.max_elements = 0;
            guard.dirty = false;

            // Dropping `hnsw_rs` structures can trigger Rayon parallel iterators. Run the drop
            // inside our dedicated pool to avoid touching the process-global pool.
            self.hnsw_pool.install(|| drop(old_hnsw));

            panic!("poison EmbeddingSemanticSearch index mutex for test");
        }

        #[doc(hidden)]
        pub fn __index_is_dirty_for_tests(&self) -> bool {
            self.lock_index().dirty
        }

        #[doc(hidden)]
        pub fn __index_rebuild_count_for_tests(&self) -> usize {
            self.lock_index().rebuild_count
        }

        #[doc(hidden)]
        pub fn __index_max_elements_for_tests(&self) -> usize {
            self.lock_index().max_elements
        }

        #[doc(hidden)]
        pub fn __index_id_to_doc_len_for_tests(&self) -> usize {
            self.lock_index().id_to_doc.len()
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

    const MAX_SNIPPET_CHARS: usize = 2_000;
    const MAX_EMBED_TEXT_CHARS: usize = 2_000;

    fn extract_java_symbols(
        path: &PathBuf,
        source: &str,
    ) -> Vec<(Range<usize>, String, String, String)> {
        use nova_syntax::{
            parse_java, AnnotationTypeDeclaration, AstNode, ClassDeclaration, CompilationUnit,
            EnumDeclaration, FieldDeclaration, InterfaceDeclaration, MethodDeclaration,
            RecordDeclaration, SyntaxKind,
        };

        let parse = parse_java(source);
        let root = parse.syntax();

        let package = CompilationUnit::cast(root.clone())
            .and_then(|unit| unit.package())
            .and_then(|pkg| pkg.name())
            .map(|name| name.text())
            .unwrap_or_default();

        let mut out = Vec::new();

        for node in root.descendants() {
            match node.kind() {
                SyntaxKind::MethodDeclaration => {
                    let byte_range = node_byte_range(source, &node);
                    let start = byte_range.start;
                    let end = byte_range.end;

                    let method_text = source[start..end].trim();
                    if method_text.is_empty() {
                        continue;
                    }

                    let doc = find_doc_comment_before_offset(source, start)
                        .map(|doc| clean_doc_comment(&doc))
                        .unwrap_or_default();

                    let (method_name, method_signature) =
                        if let Some(method) = MethodDeclaration::cast(node.clone()) {
                            let name = method
                                .name_token()
                                .map(|tok| tok.text().to_string())
                                .unwrap_or_default();
                            let return_ty = method
                                .return_type()
                                .map(|ty| normalize_whitespace(&ty.syntax().text().to_string()))
                                .unwrap_or_else(|| "void".to_string());

                            let params = method
                                .parameters()
                                .map(|param| {
                                    let ty = param
                                        .ty()
                                        .map(|ty| normalize_whitespace(&ty.syntax().text().to_string()))
                                        .unwrap_or_default();
                                    let name = param
                                        .name_token()
                                        .map(|tok| tok.text().to_string())
                                        .unwrap_or_default();

                                    match (ty.is_empty(), name.is_empty()) {
                                        (false, false) => format!("{ty} {name}"),
                                        (false, true) => ty,
                                        (true, false) => name,
                                        (true, true) => String::new(),
                                    }
                                })
                                .filter(|param| !param.is_empty())
                                .collect::<Vec<_>>()
                                .join(", ");

                            let signature = if name.is_empty() {
                                normalize_whitespace(&extract_signature(method_text))
                            } else {
                                format!("{return_ty} {name}({params})").trim().to_string()
                            };

                            (name, signature)
                        } else {
                            (
                                String::new(),
                                normalize_whitespace(&extract_signature(method_text)),
                            )
                        };

                    let enclosing_types = enclosing_type_names(&node);
                    let enclosing = enclosing_types.join(".");

                    let body = extract_body_preview(method_text);
                    let snippet = preview(method_text, MAX_SNIPPET_CHARS);

                    let mut embed_lines = Vec::new();
                    embed_lines.push(format!("path: {}", path.to_string_lossy()));
                    if !package.is_empty() {
                        embed_lines.push(format!("package: {package}"));
                    }
                    if !enclosing.is_empty() {
                        embed_lines.push(format!("enclosing: {enclosing}"));
                    }
                    embed_lines.push("kind: method".to_string());
                    if !method_name.is_empty() {
                        embed_lines.push(format!("name: {method_name}"));
                    }
                    if !method_signature.is_empty() {
                        embed_lines.push(format!("signature: {method_signature}"));
                    }
                    if !doc.is_empty() {
                        embed_lines.push("doc:".to_string());
                        embed_lines.push(doc);
                    }
                    if !body.is_empty() {
                        embed_lines.push("body:".to_string());
                        embed_lines.push(body);
                    }

                    let embed_text = preview(&embed_lines.join("\n"), MAX_EMBED_TEXT_CHARS);

                    out.push((start..end, "method".to_string(), snippet, embed_text));
                }
                SyntaxKind::FieldDeclaration => {
                    let Some(field) = FieldDeclaration::cast(node.clone()) else {
                        continue;
                    };

                    let field_decl_range = node_byte_range(source, &node);
                    let field_decl_start = field_decl_range.start;
                    let doc = find_doc_comment_before_offset(source, field_decl_start)
                        .map(|doc| clean_doc_comment(&doc))
                        .unwrap_or_default();

                    let enclosing_types = enclosing_type_names(&node);
                    let enclosing = enclosing_types.join(".");

                    let field_ty = field
                        .ty()
                        .map(|ty| normalize_whitespace(&ty.syntax().text().to_string()))
                        .unwrap_or_default();

                    for declarator in field.declarators() {
                        let Some(name_tok) = declarator.name_token() else {
                            continue;
                        };
                        let field_name = name_tok.text().to_string();
                        if field_name.is_empty() {
                            continue;
                        }

                        let decl_range = node_byte_range(source, declarator.syntax());
                        let signature = if field_ty.is_empty() {
                            field_name.clone()
                        } else {
                            format!("{field_ty} {field_name}")
                        };

                        let snippet = preview(&signature, MAX_SNIPPET_CHARS);

                        let mut embed_lines = Vec::new();
                        embed_lines.push(format!("path: {}", path.to_string_lossy()));
                        if !package.is_empty() {
                            embed_lines.push(format!("package: {package}"));
                        }
                        if !enclosing.is_empty() {
                            embed_lines.push(format!("enclosing: {enclosing}"));
                        }
                        embed_lines.push("kind: field".to_string());
                        embed_lines.push(format!("name: {field_name}"));
                        if !field_ty.is_empty() {
                            embed_lines.push(format!("type: {field_ty}"));
                        }
                        embed_lines.push(format!("signature: {signature}"));
                        if !doc.is_empty() {
                            embed_lines.push("doc:".to_string());
                            embed_lines.push(doc.clone());
                        }

                        let embed_text = preview(&embed_lines.join("\n"), MAX_EMBED_TEXT_CHARS);
                        out.push((decl_range, "field".to_string(), snippet, embed_text));
                    }
                }
                SyntaxKind::ClassDeclaration
                | SyntaxKind::InterfaceDeclaration
                | SyntaxKind::EnumDeclaration
                | SyntaxKind::RecordDeclaration
                | SyntaxKind::AnnotationTypeDeclaration => {
                    let (type_name, type_kind) = match node.kind() {
                        SyntaxKind::ClassDeclaration => ClassDeclaration::cast(node.clone())
                            .and_then(|decl| decl.name_token())
                            .map(|tok| (tok.text().to_string(), "class")),
                        SyntaxKind::InterfaceDeclaration => InterfaceDeclaration::cast(node.clone())
                            .and_then(|decl| decl.name_token())
                            .map(|tok| (tok.text().to_string(), "interface")),
                        SyntaxKind::EnumDeclaration => EnumDeclaration::cast(node.clone())
                            .and_then(|decl| decl.name_token())
                            .map(|tok| (tok.text().to_string(), "enum")),
                        SyntaxKind::RecordDeclaration => RecordDeclaration::cast(node.clone())
                            .and_then(|decl| decl.name_token())
                            .map(|tok| (tok.text().to_string(), "record")),
                        SyntaxKind::AnnotationTypeDeclaration => {
                            AnnotationTypeDeclaration::cast(node.clone())
                                .and_then(|decl| decl.name_token())
                                .map(|tok| (tok.text().to_string(), "annotation"))
                        }
                        _ => None,
                    }
                    .unwrap_or_default();

                    if type_name.is_empty() {
                        continue;
                    }

                    let byte_range = node_byte_range(source, &node);
                    let start = byte_range.start;
                    let end = byte_range.end;

                    let type_text = source[start..end].trim();
                    if type_text.is_empty() {
                        continue;
                    }

                    let doc = find_doc_comment_before_offset(source, start)
                        .map(|doc| clean_doc_comment(&doc))
                        .unwrap_or_default();
                    let declaration = normalize_whitespace(&extract_signature(type_text));

                    let mut qualified_parts = enclosing_type_names(&node);
                    qualified_parts.push(type_name.clone());
                    let qualified_name = qualified_parts.join(".");

                    let snippet = preview(&declaration, MAX_SNIPPET_CHARS);

                    let mut embed_lines = Vec::new();
                    embed_lines.push(format!("path: {}", path.to_string_lossy()));
                    if !package.is_empty() {
                        embed_lines.push(format!("package: {package}"));
                    }
                    embed_lines.push("kind: type".to_string());
                    embed_lines.push(format!("type: {qualified_name}"));
                    if !type_kind.is_empty() {
                        embed_lines.push(format!("type_kind: {type_kind}"));
                    }
                    if !declaration.is_empty() {
                        embed_lines.push(format!("declaration: {declaration}"));
                    }
                    if !doc.is_empty() {
                        embed_lines.push("doc:".to_string());
                        embed_lines.push(doc);
                    }

                    let embed_text = preview(&embed_lines.join("\n"), MAX_EMBED_TEXT_CHARS);
                    out.push((start..end, "type".to_string(), snippet, embed_text));
                }
                _ => continue,
            }

            // Continue traversal
        }

        out
    }

    fn node_byte_range(source: &str, node: &nova_syntax::SyntaxNode) -> Range<usize> {
        let range = node.text_range();
        let start = u32::from(range.start()) as usize;
        let end = u32::from(range.end()) as usize;
        let start = start.min(source.len());
        let end = end.min(source.len()).max(start);
        start..end
    }

    fn enclosing_type_names(node: &nova_syntax::SyntaxNode) -> Vec<String> {
        use nova_syntax::{
            AnnotationTypeDeclaration, AstNode, ClassDeclaration, EnumDeclaration,
            InterfaceDeclaration, RecordDeclaration, SyntaxKind,
        };

        let mut out = Vec::new();
        for ancestor in node.ancestors().skip(1) {
            let name = match ancestor.kind() {
                SyntaxKind::ClassDeclaration => ClassDeclaration::cast(ancestor.clone())
                    .and_then(|decl| decl.name_token())
                    .map(|tok| tok.text().to_string()),
                SyntaxKind::InterfaceDeclaration => InterfaceDeclaration::cast(ancestor.clone())
                    .and_then(|decl| decl.name_token())
                    .map(|tok| tok.text().to_string()),
                SyntaxKind::EnumDeclaration => EnumDeclaration::cast(ancestor.clone())
                    .and_then(|decl| decl.name_token())
                    .map(|tok| tok.text().to_string()),
                SyntaxKind::RecordDeclaration => RecordDeclaration::cast(ancestor.clone())
                    .and_then(|decl| decl.name_token())
                    .map(|tok| tok.text().to_string()),
                SyntaxKind::AnnotationTypeDeclaration => AnnotationTypeDeclaration::cast(ancestor)
                    .and_then(|decl| decl.name_token())
                    .map(|tok| tok.text().to_string()),
                _ => None,
            };

            if let Some(name) = name {
                if !name.is_empty() {
                    out.push(name);
                }
            }
        }

        out.reverse();
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

    fn normalize_whitespace(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut prev_space = true;
        for ch in text.chars() {
            if ch.is_whitespace() {
                if !prev_space {
                    out.push(' ');
                    prev_space = true;
                }
            } else {
                out.push(ch);
                prev_space = false;
            }
        }
        out.trim().to_string()
    }

    fn clean_doc_comment(doc: &str) -> String {
        let doc = doc.trim();
        let doc = doc.strip_prefix("/**").unwrap_or(doc);
        let doc = doc.strip_suffix("*/").unwrap_or(doc);
        let mut out_lines = Vec::new();
        for line in doc.lines() {
            let line = line.trim();
            let line = line.strip_prefix('*').unwrap_or(line).trim_start();
            let line = normalize_whitespace(line);
            if !line.is_empty() {
                out_lines.push(line);
            }
        }

        out_lines.join("\n")
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

    #[cfg(test)]
    mod tests {
        use super::*;

        #[derive(Debug, Clone, Copy)]
        struct VariableDimsEmbedder;

        impl Embedder for VariableDimsEmbedder {
            fn embed(&self, text: &str) -> Result<Vec<f32>, AiError> {
                if text.contains("EMPTY") {
                    return Ok(Vec::new());
                }

                // Return different dimensionalities based on content so rebuild_index_locked must
                // skip some docs and size the HNSW index based on the remaining inserted vectors.
                if text.contains("DIM3") {
                    Ok(vec![1.0, 0.0, 0.0])
                } else {
                    Ok(vec![1.0, 0.0])
                }
            }
        }

        #[test]
        fn rebuild_index_counts_only_inserted_docs() {
            let mut search = EmbeddingSemanticSearch::new(VariableDimsEmbedder);

            search.index_file(PathBuf::from("a.txt"), "DIM3 hello".to_string());
            search.index_file(PathBuf::from("b.txt"), "DIM2 skip-me".to_string());
            search.index_file(PathBuf::from("c.txt"), "DIM3 hello again".to_string());

            // Empty embeddings are skipped during indexing.
            search.index_file(PathBuf::from("d.txt"), "EMPTY".to_string());

            let results = search.search("DIM3 hello");
            assert!(!results.is_empty());
            assert_eq!(results[0].path, PathBuf::from("a.txt"));

            // The mismatched-dimension doc should not be returned.
            assert!(!results.iter().any(|r| r.path == PathBuf::from("b.txt")));

            let index = search.lock_index();
            assert_eq!(
                index.id_to_doc.len(),
                2,
                "only docs with matching embedding dimensions should be inserted"
            );
            assert_eq!(index.max_elements, index.id_to_doc.len());

            let total_docs = search
                .docs_by_path
                .values()
                .map(|docs| docs.len())
                .sum::<usize>();
            assert_eq!(total_docs, 3);
            assert!(
                index.max_elements < total_docs,
                "HNSW max_elements should be based on inserted docs, not raw extracted docs"
            );
        }

        #[test]
        fn index_file_skips_embedding_when_budget_cannot_fit_any_vectors() {
            #[derive(Debug, Clone, Copy)]
            struct PanicEmbedder;

            impl Embedder for PanicEmbedder {
                fn embed(&self, _text: &str) -> Result<Vec<f32>, AiError> {
                    panic!("embed should not be called when budget cannot fit any embeddings");
                }
            }

            // `with_max_memory_bytes` clamps `0` -> `1`, which is still too small to store even a
            // single `f32` (4 bytes). Indexing should therefore skip embedding entirely.
            let mut search = EmbeddingSemanticSearch::new(PanicEmbedder).with_max_memory_bytes(1);
            search.index_file(PathBuf::from("a.txt"), "hello".to_string());
            assert!(search.docs_by_path.is_empty());
        }

        #[test]
        fn semantic_search_records_timeout_metrics_when_embedding_fails() {
            let _guard = crate::test_support::metrics_lock()
                .lock()
                .expect("metrics lock poisoned");
            let metrics = nova_metrics::MetricsRegistry::global();

            let before = metrics.snapshot();
            let before_index_req = before
                .methods
                .get(super::super::AI_SEMANTIC_SEARCH_INDEX_FILE_METRIC)
                .map(|m| m.request_count)
                .unwrap_or(0);
            let before_index_timeout = before
                .methods
                .get(super::super::AI_SEMANTIC_SEARCH_INDEX_FILE_METRIC)
                .map(|m| m.timeout_count)
                .unwrap_or(0);
            let before_search_req = before
                .methods
                .get(super::super::AI_SEMANTIC_SEARCH_SEARCH_METRIC)
                .map(|m| m.request_count)
                .unwrap_or(0);
            let before_search_timeout = before
                .methods
                .get(super::super::AI_SEMANTIC_SEARCH_SEARCH_METRIC)
                .map(|m| m.timeout_count)
                .unwrap_or(0);

            #[derive(Debug, Clone, Copy)]
            struct TimeoutEmbedder;

            impl Embedder for TimeoutEmbedder {
                fn embed(&self, _text: &str) -> Result<Vec<f32>, AiError> {
                    Err(AiError::Timeout)
                }

                fn embed_batch(&self, _inputs: &[String]) -> Result<Vec<Vec<f32>>, AiError> {
                    Err(AiError::Timeout)
                }
            }

            let mut search = EmbeddingSemanticSearch::new(TimeoutEmbedder);
            search.index_file(PathBuf::from("a.txt"), "hello world".to_string());
            let results = search.search("query");
            assert!(
                results.is_empty(),
                "expected empty results on embed timeout"
            );

            let after = metrics.snapshot();
            let after_index_req = after
                .methods
                .get(super::super::AI_SEMANTIC_SEARCH_INDEX_FILE_METRIC)
                .map(|m| m.request_count)
                .unwrap_or(0);
            let after_index_timeout = after
                .methods
                .get(super::super::AI_SEMANTIC_SEARCH_INDEX_FILE_METRIC)
                .map(|m| m.timeout_count)
                .unwrap_or(0);
            let after_search_req = after
                .methods
                .get(super::super::AI_SEMANTIC_SEARCH_SEARCH_METRIC)
                .map(|m| m.request_count)
                .unwrap_or(0);
            let after_search_timeout = after
                .methods
                .get(super::super::AI_SEMANTIC_SEARCH_SEARCH_METRIC)
                .map(|m| m.timeout_count)
                .unwrap_or(0);

            assert!(
                after_index_req >= before_index_req.saturating_add(1),
                "expected index_file request_count to increment"
            );
            assert!(
                after_index_timeout >= before_index_timeout.saturating_add(1),
                "expected index_file timeout_count to increment on embed timeout"
            );
            assert!(
                after_search_req >= before_search_req.saturating_add(1),
                "expected search request_count to increment"
            );
            assert!(
                after_search_timeout >= before_search_timeout.saturating_add(1),
                "expected search timeout_count to increment on embed timeout"
            );
        }
    }
}

#[cfg(feature = "embeddings")]
pub use embeddings::{Embedder, EmbeddingSemanticSearch, HashEmbedder};

#[cfg(all(feature = "embeddings", feature = "embeddings-local"))]
pub use embeddings::LocalEmbedder;
