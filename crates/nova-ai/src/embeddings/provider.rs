use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::json;
use std::future::Future;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::client::validate_local_only_url;
use crate::llm_privacy::PrivacyFilter;
use crate::privacy::redact_file_paths;
use crate::AiError;
use nova_config::{AiConfig, AiProviderKind};
use nova_metrics::MetricsRegistry;

use super::cache::{EmbeddingCacheKey as MemoryCacheKey, EmbeddingCacheKeyBuilder, EmbeddingVectorCache};
use super::disk_cache::{
    DiskEmbeddingCache, EmbeddingCacheKey as DiskCacheKey, DISK_CACHE_NAMESPACE_V1,
};
use super::{EmbeddingInputKind, EmbeddingsClient};

const AI_EMBEDDINGS_RETRY_METRIC: &str = "ai/embeddings/retry";

#[derive(Debug, Clone)]
struct RetryConfig {
    max_retries: usize,
    initial_backoff: Duration,
    max_backoff: Duration,
}

fn retry_config_from_provider_config(config: &AiConfig) -> RetryConfig {
    RetryConfig {
        max_retries: config.provider.retry_max_retries,
        initial_backoff: Duration::from_millis(config.provider.retry_initial_backoff_ms),
        max_backoff: Duration::from_millis(config.provider.retry_max_backoff_ms),
    }
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 2,
            initial_backoff: Duration::from_millis(200),
            max_backoff: Duration::from_secs(2),
        }
    }
}

fn should_retry(err: &AiError) -> bool {
    match err {
        AiError::Cancelled => false,
        AiError::Timeout => true,
        AiError::Http(err) => {
            if err.is_timeout() || err.is_connect() {
                return true;
            }
            let Some(status) = err.status() else {
                // Network errors without a status are generally worth retrying.
                return true;
            };
            status.as_u16() == 408 || status.as_u16() == 429 || status.is_server_error()
        }
        _ => false,
    }
}

async fn backoff_sleep(
    retry: &RetryConfig,
    attempt: usize,
    max_delay: Duration,
    cancel: &CancellationToken,
) -> Result<(), AiError> {
    let factor = 2u32.saturating_pow((attempt.saturating_sub(1)).min(16) as u32);
    let mut delay = retry.initial_backoff.saturating_mul(factor);
    if delay > retry.max_backoff {
        delay = retry.max_backoff;
    }
    if delay > max_delay {
        delay = max_delay;
    }

    tokio::select! {
        _ = cancel.cancelled() => Err(AiError::Cancelled),
        _ = tokio::time::sleep(delay) => Ok(()),
    }
}

async fn with_retry<T, F, Fut>(
    provider_label: &'static str,
    timeout: Duration,
    retry: &RetryConfig,
    cancel: &CancellationToken,
    mut op: F,
) -> Result<T, AiError>
where
    F: FnMut(Duration) -> Fut,
    Fut: Future<Output = Result<T, AiError>>,
{
    let metrics = MetricsRegistry::global();
    let operation_start = Instant::now();
    let mut attempt = 0usize;

    loop {
        if cancel.is_cancelled() {
            return Err(AiError::Cancelled);
        }

        let remaining = timeout.saturating_sub(operation_start.elapsed());
        if remaining == Duration::ZERO {
            return Err(AiError::Timeout);
        }

        if attempt > 0 {
            metrics.record_request(AI_EMBEDDINGS_RETRY_METRIC, Duration::from_micros(1));
        }

        let result = tokio::select! {
            _ = cancel.cancelled() => Err(AiError::Cancelled),
            res = tokio::time::timeout(remaining, op(remaining)) => match res {
                Ok(out) => out,
                Err(_) => Err(AiError::Timeout),
            },
        };

        match result {
            Ok(out) => return Ok(out),
            Err(err) if attempt < retry.max_retries && should_retry(&err) => {
                attempt += 1;
                tracing::warn!(
                    provider = provider_label,
                    attempt,
                    error = %err,
                    "embeddings request failed, retrying"
                );

                let remaining = timeout.saturating_sub(operation_start.elapsed());
                if remaining == Duration::ZERO {
                    return Err(AiError::Timeout);
                }
                backoff_sleep(retry, attempt, remaining, cancel).await?;
            }
            Err(err) => return Err(err),
        }
    }
}

pub(super) fn provider_embeddings_client_from_config(
    config: &AiConfig,
    max_memory_bytes: usize,
) -> Result<Box<dyn EmbeddingsClient>, AiError> {
    if config.privacy.local_only {
        match &config.provider.kind {
            AiProviderKind::Ollama | AiProviderKind::OpenAiCompatible | AiProviderKind::Http => {
                if let Err(err) = validate_local_only_url(&config.provider.url) {
                    tracing::warn!(
                        target = "nova.ai",
                        provider_kind = ?config.provider.kind,
                        url = %config.provider.url,
                        ?err,
                        "ai.privacy.local_only=true forbids provider-backed embeddings to non-loopback urls; falling back to hash embeddings"
                    );
                    return Ok(Box::new(super::LocalEmbeddingsClient::new(max_memory_bytes)));
                }
            }
            other => {
                tracing::warn!(
                    target = "nova.ai",
                    provider_kind = ?other,
                    "ai.privacy.local_only=true forbids provider-backed embeddings for cloud providers; falling back to hash embeddings"
                );
                return Ok(Box::new(super::LocalEmbeddingsClient::new(max_memory_bytes)));
            }
        }
    }

    let timeout = config
        .embeddings
        .timeout_ms
        .map(Duration::from_millis)
        .unwrap_or_else(|| config.provider.timeout());
    let model = config
        .embeddings
        .model
        .clone()
        .unwrap_or_else(|| config.provider.model.clone());
    let batch_size = config.embeddings.batch_size.max(1);
    let retry = retry_config_from_provider_config(config);
    let redact_paths = !config.privacy.local_only && !config.privacy.include_file_paths;

    let privacy = Arc::new(PrivacyFilter::new(&config.privacy)?);

    // `ai.embeddings.model_dir` is used for the on-disk embedding cache. Build it lazily so we
    // don't fail in configurations that already fall back to hash embeddings (missing API keys,
    // unsupported providers, etc.).
    let disk_cache = || -> Result<Option<Arc<DiskEmbeddingCache>>, AiError> {
        if config.embeddings.model_dir.as_os_str().is_empty() {
            return Ok(None);
        }

        Ok(Some(Arc::new(
            DiskEmbeddingCache::new(config.embeddings.model_dir.clone()).map_err(|err| {
                AiError::InvalidConfig(format!(
                    "failed to create ai.embeddings.model_dir {}: {err}",
                    config.embeddings.model_dir.display()
                ))
            })?,
        )))
    };

    match &config.provider.kind {
        AiProviderKind::AzureOpenAi => {
            let Some(api_key) = config.api_key.clone() else {
                tracing::warn!(
                    target = "nova.ai",
                    "Azure OpenAI embeddings require ai.api_key; falling back to hash embeddings"
                );
                return Ok(Box::new(super::LocalEmbeddingsClient::new(max_memory_bytes)));
            };
            let Some(deployment) = config
                .embeddings
                .model
                .clone()
                .or_else(|| config.provider.azure_deployment.clone())
            else {
                tracing::warn!(
                    target = "nova.ai",
                    "Azure OpenAI embeddings require ai.provider.azure_deployment; falling back to hash embeddings"
                );
                return Ok(Box::new(super::LocalEmbeddingsClient::new(max_memory_bytes)));
            };
             if deployment.trim().is_empty() {
                 tracing::warn!(
                     target = "nova.ai",
                     "Azure OpenAI embeddings require a non-empty deployment name; falling back to hash embeddings"
                 );
                return Ok(Box::new(super::LocalEmbeddingsClient::new(max_memory_bytes)));
             }
            let api_version = config
                .provider
                .azure_api_version
                .clone()
                .unwrap_or_else(|| "2024-02-01".to_string());

            let base = match AzureOpenAiEmbeddingsClient::new(
                config.provider.url.clone(),
                api_key,
                deployment.clone(),
                api_version,
                timeout,
                batch_size,
                retry.clone(),
            ) {
                Ok(base) => base,
                Err(err) => {
                    tracing::warn!(
                        target = "nova.ai",
                        ?err,
                        "failed to build Azure OpenAI embeddings client; falling back to hash embeddings"
                    );
                    return Ok(Box::new(super::LocalEmbeddingsClient::new(max_memory_bytes)));
                }
            };

            let endpoint_id = match base.endpoint_id() {
                Ok(id) => id,
                Err(err) => {
                    tracing::warn!(
                        target = "nova.ai",
                        ?err,
                        "failed to compute Azure OpenAI embeddings endpoint id; falling back to hash embeddings"
                    );
                    return Ok(Box::new(super::LocalEmbeddingsClient::new(max_memory_bytes)));
                }
            };

            let model_id = match cached_model_id_for_azure(config) {
                Ok(id) => id,
                Err(err) => {
                    tracing::warn!(
                        target = "nova.ai",
                        ?err,
                        "failed to compute Azure OpenAI embeddings cache key; falling back to hash embeddings"
                    );
                    return Ok(Box::new(super::LocalEmbeddingsClient::new(max_memory_bytes)));
                }
            };

            let disk_cache = disk_cache()?;
            Ok(Box::new(CachedEmbeddingsClient::new(
                Box::new(base),
                "azure_open_ai",
                endpoint_id,
                model_id,
                max_memory_bytes,
                disk_cache,
                privacy.clone(),
                redact_paths,
            )))
        }
        AiProviderKind::OpenAi => {
            let Some(api_key) = config.api_key.clone() else {
                tracing::warn!(
                    target = "nova.ai",
                    "OpenAI embeddings require ai.api_key; falling back to hash embeddings"
                );
                return Ok(Box::new(super::LocalEmbeddingsClient::new(max_memory_bytes)));
            };

            let base = match OpenAiCompatibleEmbeddingsClient::new(
                config.provider.url.clone(),
                model.clone(),
                Some(api_key),
                timeout,
                batch_size,
                retry.clone(),
            ) {
                Ok(base) => base,
                Err(err) => {
                    tracing::warn!(
                        target = "nova.ai",
                        ?err,
                        "failed to build OpenAI embeddings client; falling back to hash embeddings"
                    );
                    return Ok(Box::new(super::LocalEmbeddingsClient::new(max_memory_bytes)));
                }
            };

            let endpoint_id = match base.embeddings_endpoint_id() {
                Ok(id) => id,
                Err(err) => {
                    tracing::warn!(
                        target = "nova.ai",
                        ?err,
                        "failed to compute OpenAI embeddings endpoint id; falling back to hash embeddings"
                    );
                    return Ok(Box::new(super::LocalEmbeddingsClient::new(max_memory_bytes)));
                }
            };

            let disk_cache = disk_cache()?;
            Ok(Box::new(CachedEmbeddingsClient::new(
                Box::new(base),
                "openai",
                endpoint_id,
                model,
                max_memory_bytes,
                disk_cache,
                privacy.clone(),
                redact_paths,
            )))
        }
        AiProviderKind::OpenAiCompatible => {
            let base = match OpenAiCompatibleEmbeddingsClient::new(
                config.provider.url.clone(),
                model.clone(),
                config.api_key.clone(),
                timeout,
                batch_size,
                retry.clone(),
            ) {
                Ok(base) => base,
                Err(err) => {
                    tracing::warn!(
                        target = "nova.ai",
                        ?err,
                        "failed to build embeddings client; falling back to hash embeddings"
                    );
                    return Ok(Box::new(super::LocalEmbeddingsClient::new(max_memory_bytes)));
                }
            };

            let endpoint_id = match base.embeddings_endpoint_id() {
                Ok(id) => id,
                Err(err) => {
                    tracing::warn!(
                        target = "nova.ai",
                        ?err,
                        "failed to compute embeddings endpoint id; falling back to hash embeddings"
                    );
                    return Ok(Box::new(super::LocalEmbeddingsClient::new(max_memory_bytes)));
                }
            };

            let disk_cache = disk_cache()?;
            Ok(Box::new(CachedEmbeddingsClient::new(
                Box::new(base),
                "openai_compatible",
                endpoint_id,
                model,
                max_memory_bytes,
                disk_cache,
                privacy.clone(),
                redact_paths,
            )))
        }
        AiProviderKind::Http => {
            let base = match OpenAiCompatibleEmbeddingsClient::new(
                config.provider.url.clone(),
                model.clone(),
                config.api_key.clone(),
                timeout,
                batch_size,
                retry.clone(),
            ) {
                Ok(base) => base,
                Err(err) => {
                    tracing::warn!(
                        target = "nova.ai",
                        ?err,
                        "failed to build HTTP embeddings client; falling back to hash embeddings"
                    );
                    return Ok(Box::new(super::LocalEmbeddingsClient::new(max_memory_bytes)));
                }
            };

            let endpoint_id = match base.embeddings_endpoint_id() {
                Ok(id) => id,
                Err(err) => {
                    tracing::warn!(
                        target = "nova.ai",
                        ?err,
                        "failed to compute HTTP embeddings endpoint id; falling back to hash embeddings"
                    );
                    return Ok(Box::new(super::LocalEmbeddingsClient::new(max_memory_bytes)));
                }
            };

            let disk_cache = disk_cache()?;
            Ok(Box::new(CachedEmbeddingsClient::new(
                Box::new(base),
                "http",
                endpoint_id,
                model,
                max_memory_bytes,
                disk_cache,
                privacy.clone(),
                redact_paths,
            )))
        }
        AiProviderKind::Ollama => {
            let base = match OllamaEmbeddingsClient::new(
                config.provider.url.clone(),
                model.clone(),
                timeout,
                batch_size,
                retry.clone(),
            ) {
                Ok(base) => base,
                Err(err) => {
                    tracing::warn!(
                        target = "nova.ai",
                        ?err,
                        "failed to build Ollama embeddings client; falling back to hash embeddings"
                    );
                    return Ok(Box::new(super::LocalEmbeddingsClient::new(max_memory_bytes)));
                }
            };

            let endpoint_id = match base.endpoint_id() {
                Ok(id) => id,
                Err(err) => {
                    tracing::warn!(
                        target = "nova.ai",
                        ?err,
                        "failed to compute Ollama embeddings endpoint id; falling back to hash embeddings"
                    );
                    return Ok(Box::new(super::LocalEmbeddingsClient::new(max_memory_bytes)));
                }
            };

            let disk_cache = disk_cache()?;
            Ok(Box::new(CachedEmbeddingsClient::new(
                Box::new(base),
                "ollama",
                endpoint_id,
                model,
                max_memory_bytes,
                disk_cache,
                privacy.clone(),
                redact_paths,
            )))
        }
        other => {
            tracing::warn!(
                target = "nova.ai",
                provider_kind = ?other,
                "ai.embeddings.backend=provider is not supported for this provider; falling back to hash embeddings"
            );
            Ok(Box::new(super::LocalEmbeddingsClient::new(max_memory_bytes)))
        }
    }
}

fn cached_model_id_for_azure(config: &AiConfig) -> Result<String, AiError> {
    // `AzureOpenAiEmbeddingsClient` is keyed by deployment.
    //
    // When `ai.embeddings.model` is set we treat it as a deployment override (so users can point
    // chat completions and embeddings at different Azure deployments).
    if let Some(model) = config.embeddings.model.clone() {
        if model.trim().is_empty() {
            return Err(AiError::InvalidConfig(
                "ai.embeddings.model must be non-empty when set".into(),
            ));
        }
        return Ok(model);
    }

    // If we don't have a deployment, the config is already invalid; treat it as an error here so
    // the cache key remains deterministic.
    config
        .provider
        .azure_deployment
        .clone()
        .ok_or_else(|| {
            AiError::InvalidConfig(
                "Azure OpenAI embeddings require ai.provider.azure_deployment".into(),
            )
        })
}

struct CachedEmbeddingsClient {
    inner: Box<dyn EmbeddingsClient>,
    backend_id: &'static str,
    endpoint_id: String,
    model: String,
    memory_cache: EmbeddingVectorCache,
    disk_cache: Option<Arc<DiskEmbeddingCache>>,
    privacy: Arc<PrivacyFilter>,
    redact_paths: bool,
}

impl CachedEmbeddingsClient {
    fn new(
        inner: Box<dyn EmbeddingsClient>,
        backend_id: &'static str,
        endpoint_id: String,
        model: String,
        max_memory_bytes: usize,
        disk_cache: Option<Arc<DiskEmbeddingCache>>,
        privacy: Arc<PrivacyFilter>,
        redact_paths: bool,
    ) -> Self {
        Self {
            inner,
            backend_id,
            endpoint_id,
            model,
            memory_cache: EmbeddingVectorCache::new(max_memory_bytes),
            disk_cache,
            privacy,
            redact_paths,
        }
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
        let mut builder = EmbeddingCacheKeyBuilder::new("nova-ai-embeddings-memory-cache-v1");
        builder.push_str(self.backend_id);
        builder.push_str(&self.endpoint_id);
        builder.push_str(&self.model);
        builder.push_str(input);
        builder.finish()
    }
}

#[async_trait]
impl EmbeddingsClient for CachedEmbeddingsClient {
    async fn embed(
        &self,
        input: &[String],
        kind: EmbeddingInputKind,
        cancel: CancellationToken,
    ) -> Result<Vec<Vec<f32>>, AiError> {
        if input.is_empty() {
            return Ok(Vec::new());
        }
        if cancel.is_cancelled() {
            return Err(AiError::Cancelled);
        }

        // Sanitize all inputs with the same session so anonymization/redaction is stable
        // within a single embeddings request batch.
        let mut session = self.privacy.new_session();
        let sanitized_input: Vec<String> = input
            .iter()
            .map(|text| {
                let sanitized = match kind {
                    EmbeddingInputKind::Document => {
                        self.privacy.sanitize_code_text(&mut session, text)
                    }
                    EmbeddingInputKind::Query => self.privacy.sanitize_prompt_text(&mut session, text),
                };
                if self.redact_paths {
                    redact_file_paths(&sanitized)
                } else {
                    sanitized
                }
            })
            .collect();

        let mut out = vec![None::<Vec<f32>>; input.len()];
        let mut miss_indices = Vec::new();
        let mut miss_disk_keys = Vec::new();
        let mut miss_memory_keys = Vec::new();
        let mut miss_inputs = Vec::new();

        for (idx, text) in sanitized_input.iter().enumerate() {
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

        // Disk cache lookups.
        if let Some(disk) = self.disk_cache.clone() {
            let disk_keys = miss_disk_keys.clone();
            let expected = disk_keys.len();
            let spawn_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe({
                let disk = disk.clone();
                let disk_keys = disk_keys.clone();
                move || {
                    tokio::task::spawn_blocking(move || {
                        disk_keys
                            .into_iter()
                            .map(|key| disk.load(key).ok().flatten())
                            .collect::<Vec<_>>()
                    })
                }
            }));

            let disk_hits = match spawn_result {
                Ok(handle) => match handle.await {
                    Ok(hits) if hits.len() == expected => hits,
                    _ => vec![None; expected],
                },
                Err(_) => disk_keys
                    .into_iter()
                    .map(|key| disk.load(key).ok().flatten())
                    .collect::<Vec<_>>(),
            };

            let mut still_indices = Vec::new();
            let mut still_disk_keys = Vec::new();
            let mut still_memory_keys = Vec::new();
            let mut still_inputs = Vec::new();

            for (((idx, memory_key), disk_key), hit) in miss_indices
                .into_iter()
                .zip(miss_memory_keys.into_iter())
                .zip(miss_disk_keys.into_iter())
                .zip(disk_hits.into_iter())
            {
                if let Some(vec) = hit {
                    out[idx] = Some(vec.clone());
                    self.memory_cache.insert(memory_key, vec);
                } else {
                    still_indices.push(idx);
                    still_memory_keys.push(memory_key);
                    still_disk_keys.push(disk_key);
                    // `miss_inputs` matched the same order as the keys.
                }
            }

            // Rebuild still_inputs based on still_indices to preserve ordering.
            for idx in &still_indices {
                if let Some(text) = sanitized_input.get(*idx) {
                    still_inputs.push(text.clone());
                }
            }

            miss_indices = still_indices;
            miss_disk_keys = still_disk_keys;
            miss_memory_keys = still_memory_keys;
            miss_inputs = still_inputs;
        }

        // Network for cache misses.
        if !miss_inputs.is_empty() {
            let embeddings = self.inner.embed(&miss_inputs, kind, cancel).await?;
            if embeddings.len() != miss_inputs.len() {
                return Err(AiError::UnexpectedResponse(format!(
                    "embedder returned unexpected batch size: expected {}, got {}",
                    miss_inputs.len(),
                    embeddings.len()
                )));
            }

            let mut disk_inserts = Vec::with_capacity(embeddings.len());

            for (((orig_idx, memory_key), disk_key), embedding) in miss_indices
                .into_iter()
                .zip(miss_memory_keys.into_iter())
                .zip(miss_disk_keys.into_iter())
                .zip(embeddings.into_iter())
            {
                out[orig_idx] = Some(embedding.clone());
                self.memory_cache.insert(memory_key, embedding.clone());
                disk_inserts.push((disk_key, embedding));
            }

            if let Some(disk) = self.disk_cache.clone() {
                let disk_inserts = Arc::new(disk_inserts);
                let spawn_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe({
                    let disk = disk.clone();
                    let disk_inserts = disk_inserts.clone();
                    move || {
                        tokio::task::spawn_blocking(move || {
                            for (key, vec) in disk_inserts.iter() {
                                let _ = disk.store(*key, vec);
                            }
                        })
                    }
                }));

                match spawn_result {
                    Ok(handle) => {
                        let _ = handle.await;
                    }
                    Err(_) => {
                        for (key, vec) in disk_inserts.iter() {
                            let _ = disk.store(*key, vec);
                        }
                    }
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

/// Azure OpenAI embeddings provider.
#[derive(Clone)]
struct AzureOpenAiEmbeddingsClient {
    endpoint: Url,
    deployment: String,
    api_version: String,
    timeout: Duration,
    batch_size: usize,
    retry: RetryConfig,
    client: reqwest::Client,
}

impl AzureOpenAiEmbeddingsClient {
    fn new(
        endpoint: Url,
        api_key: String,
        deployment: String,
        api_version: String,
        timeout: Duration,
        batch_size: usize,
        retry: RetryConfig,
    ) -> Result<Self, AiError> {
        let mut headers = HeaderMap::new();
        headers.insert(
            "api-key",
            HeaderValue::from_str(&api_key)
                .map_err(|e| AiError::InvalidConfig(format!("invalid azure api_key: {e}")))?,
        );
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .build()?;

        Ok(Self {
            endpoint,
            deployment,
            api_version,
            timeout,
            batch_size: batch_size.max(1),
            retry,
            client,
        })
    }

    fn endpoint_id(&self) -> Result<String, AiError> {
        let mut url = self
            .endpoint
            .join(&format!(
                "openai/deployments/{}/embeddings",
                self.deployment
            ))
            .map_err(|e| AiError::InvalidConfig(e.to_string()))?;
        url.query_pairs_mut()
            .append_pair("api-version", &self.api_version);
        Ok(url.to_string())
    }

    async fn embed_once(
        &self,
        input: &[String],
        timeout: Duration,
    ) -> Result<Vec<Vec<f32>>, AiError> {
        let mut url = self
            .endpoint
            .join(&format!(
                "openai/deployments/{}/embeddings",
                self.deployment
            ))
            .map_err(|e| AiError::InvalidConfig(e.to_string()))?;
        url.query_pairs_mut()
            .append_pair("api-version", &self.api_version);

        let body = json!({
            "input": input,
        });

        let response = self
            .client
            .post(url)
            .json(&body)
            .timeout(timeout)
            .send()
            .await?
            .error_for_status()?;
        let parsed: OpenAiEmbeddingsResponse = response.json().await?;
        parse_openai_embeddings(parsed, input.len())
    }
}
#[async_trait]
impl EmbeddingsClient for AzureOpenAiEmbeddingsClient {
    async fn embed(
        &self,
        input: &[String],
        _kind: EmbeddingInputKind,
        cancel: CancellationToken,
    ) -> Result<Vec<Vec<f32>>, AiError> {
        if input.is_empty() {
            return Ok(Vec::new());
        }
        let operation_start = Instant::now();
        let batch_size = self.batch_size.max(1);

        let mut out = Vec::with_capacity(input.len());
        for chunk in input.chunks(batch_size) {
            if cancel.is_cancelled() {
                return Err(AiError::Cancelled);
            }

            let remaining = self.timeout.saturating_sub(operation_start.elapsed());
            if remaining == Duration::ZERO {
                return Err(AiError::Timeout);
            }

            let embeddings = with_retry(
                "azure_open_ai",
                remaining,
                &self.retry,
                &cancel,
                |timeout| self.embed_once(chunk, timeout),
            )
            .await?;
            out.extend(embeddings);
        }

        Ok(out)
    }
}

/// OpenAI-compatible embeddings provider.
#[derive(Clone)]
struct OpenAiCompatibleEmbeddingsClient {
    base_url: Url,
    model: String,
    timeout: Duration,
    api_key: Option<String>,
    batch_size: usize,
    retry: RetryConfig,
    client: reqwest::Client,
}

impl OpenAiCompatibleEmbeddingsClient {
    fn new(
        base_url: Url,
        model: String,
        api_key: Option<String>,
        timeout: Duration,
        batch_size: usize,
        retry: RetryConfig,
    ) -> Result<Self, AiError> {
        let mut headers = HeaderMap::new();
        if let Some(key) = api_key.as_deref() {
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {key}"))
                    .map_err(|e| AiError::InvalidConfig(e.to_string()))?,
            );
        }

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .build()?;

        Ok(Self {
            base_url,
            model,
            timeout,
            api_key,
            batch_size: batch_size.max(1),
            retry,
            client,
        })
    }

    fn endpoint(&self, path: &str) -> Result<Url, AiError> {
        // Accept both:
        // - http://localhost:8000  (we will append /v1/...)
        // - http://localhost:8000/v1  (we will append /...)
        let mut base = self.base_url.clone();
        let base_str = base.as_str().trim_end_matches('/').to_string();
        base = Url::parse(&format!("{base_str}/"))?;

        let base_path = base.path().trim_end_matches('/');
        if base_path.ends_with("/v1") {
            Ok(base.join(path.trim_start_matches('/'))?)
        } else {
            Ok(base.join(&format!("v1/{}", path.trim_start_matches('/')))?)
        }
    }

    fn embeddings_endpoint_id(&self) -> Result<String, AiError> {
        Ok(self.endpoint("/embeddings")?.to_string())
    }

    async fn embed_once(
        &self,
        input: &[String],
        timeout: Duration,
    ) -> Result<Vec<Vec<f32>>, AiError> {
        let url = self.endpoint("/embeddings")?;
        let body = json!({
            "model": &self.model,
            "input": input,
        });

        let mut request = self.client.post(url).json(&body).timeout(timeout);
        if let Some(key) = self.api_key.as_deref() {
            request = request.bearer_auth(key);
        }

        let response = request.send().await?.error_for_status()?;
        let parsed: OpenAiEmbeddingsResponse = response.json().await?;
        parse_openai_embeddings(parsed, input.len())
    }
}

#[async_trait]
impl EmbeddingsClient for OpenAiCompatibleEmbeddingsClient {
    async fn embed(
        &self,
        input: &[String],
        _kind: EmbeddingInputKind,
        cancel: CancellationToken,
    ) -> Result<Vec<Vec<f32>>, AiError> {
        if input.is_empty() {
            return Ok(Vec::new());
        }
        let operation_start = Instant::now();
        let batch_size = self.batch_size.max(1);

        let mut out = Vec::with_capacity(input.len());
        for chunk in input.chunks(batch_size) {
            if cancel.is_cancelled() {
                return Err(AiError::Cancelled);
            }

            let remaining = self.timeout.saturating_sub(operation_start.elapsed());
            if remaining == Duration::ZERO {
                return Err(AiError::Timeout);
            }

            let embeddings = with_retry(
                "openai_compatible",
                remaining,
                &self.retry,
                &cancel,
                |timeout| self.embed_once(chunk, timeout),
            )
            .await?;
            out.extend(embeddings);
        }

        Ok(out)
    }
}

#[derive(Debug, Deserialize)]
struct OpenAiEmbeddingsResponse {
    data: Vec<OpenAiEmbeddingObject>,
}

#[derive(Debug, Deserialize)]
struct OpenAiEmbeddingObject {
    embedding: Vec<f32>,
    index: usize,
}

fn parse_openai_embeddings(
    response: OpenAiEmbeddingsResponse,
    expected: usize,
) -> Result<Vec<Vec<f32>>, AiError> {
    let mut out = vec![None::<Vec<f32>>; expected];
    for item in response.data {
        if item.index >= expected {
            return Err(AiError::UnexpectedResponse(format!(
                "embeddings index {} out of range (expected < {expected})",
                item.index
            )));
        }
        if out[item.index].is_some() {
            return Err(AiError::UnexpectedResponse(format!(
                "duplicate embeddings index {}",
                item.index
            )));
        }
        out[item.index] = Some(item.embedding);
    }

    out.into_iter()
        .enumerate()
        .map(|(idx, item)| {
            item.filter(|v| !v.is_empty()).ok_or_else(|| {
                AiError::UnexpectedResponse(format!("missing embeddings data for index {idx}"))
            })
        })
        .collect()
}

/// Ollama embeddings provider.
///
/// Supports both:
/// - `/api/embed` (preferred, batch)
/// - `/api/embeddings` (legacy, one input per request)
#[derive(Clone)]
struct OllamaEmbeddingsClient {
    base_url: Url,
    model: String,
    timeout: Duration,
    batch_size: usize,
    retry: RetryConfig,
    // 0 = unknown, 1 = supported, 2 = unsupported
    embed_endpoint: Arc<AtomicU8>,
    client: reqwest::Client,
}

impl OllamaEmbeddingsClient {
    fn new(
        base_url: Url,
        model: String,
        timeout: Duration,
        batch_size: usize,
        retry: RetryConfig,
    ) -> Result<Self, AiError> {
        let client = reqwest::Client::builder().build()?;
        Ok(Self {
            base_url,
            model,
            timeout,
            batch_size: batch_size.max(1),
            retry,
            embed_endpoint: Arc::new(AtomicU8::new(0)),
            client,
        })
    }

    fn endpoint(&self, path: &str) -> Result<Url, AiError> {
        let base_str = self.base_url.as_str().trim_end_matches('/').to_string();
        let base = Url::parse(&format!("{base_str}/"))?;
        let base_path = base.path().trim_end_matches('/');
        let mut relative = path.trim_start_matches('/');
        if base_path.ends_with("/api") && relative.starts_with("api/") {
            relative = relative.trim_start_matches("api/");
        }
        Ok(base.join(relative)?)
    }

    fn endpoint_id(&self) -> Result<String, AiError> {
        Ok(self.endpoint("/api/embed")?.to_string())
    }

    async fn embed_via_embed_endpoint(
        &self,
        input: &[String],
        timeout: Duration,
    ) -> Result<Option<Vec<Vec<f32>>>, AiError> {
        let url = self.endpoint("/api/embed")?;
        let body = json!({
            "model": &self.model,
            "input": input,
        });

        let response = self
            .client
            .post(url)
            .json(&body)
            .timeout(timeout)
            .send()
            .await?;

        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }

        let response = response.error_for_status()?;
        let parsed: OllamaEmbedResponse = response.json().await?;
        let embeddings = parsed.into_embeddings().ok_or_else(|| {
            AiError::UnexpectedResponse("missing `embeddings` in Ollama /api/embed response".into())
        })?;

        if embeddings.len() != input.len() {
            return Err(AiError::UnexpectedResponse(format!(
                "Ollama /api/embed returned {} embeddings for {} inputs",
                embeddings.len(),
                input.len()
            )));
        }

        if embeddings.iter().any(|emb| emb.is_empty()) {
            return Err(AiError::UnexpectedResponse(
                "Ollama /api/embed returned empty embedding vector".into(),
            ));
        }

        Ok(Some(embeddings))
    }

    async fn embed_via_legacy_endpoint(
        &self,
        input: &[String],
        cancel: &CancellationToken,
        operation_start: Instant,
        retry: &RetryConfig,
    ) -> Result<Vec<Vec<f32>>, AiError> {
        let url = self.endpoint("/api/embeddings")?;
        let mut out = Vec::with_capacity(input.len());

        for prompt in input {
            if cancel.is_cancelled() {
                return Err(AiError::Cancelled);
            }

            let remaining = self.timeout.saturating_sub(operation_start.elapsed());
            if remaining == Duration::ZERO {
                return Err(AiError::Timeout);
            }

            let embedding = with_retry(
                "ollama",
                remaining,
                retry,
                cancel,
                |timeout| {
                    let url = url.clone();
                    async move {
                        let body = json!({
                            "model": &self.model,
                            "prompt": prompt,
                        });
                        let response = self
                            .client
                            .post(url)
                            .json(&body)
                            .timeout(timeout)
                            .send()
                            .await?
                            .error_for_status()?;

                        let parsed: OllamaEmbeddingsResponse = response.json().await?;
                        if parsed.embedding.is_empty() {
                            return Err(AiError::UnexpectedResponse(
                                "missing `embedding` in Ollama /api/embeddings response".into(),
                            ));
                        }
                        Ok(parsed.embedding)
                    }
                },
            )
            .await?;
            out.push(embedding);
        }

        Ok(out)
    }
}

#[async_trait]
impl EmbeddingsClient for OllamaEmbeddingsClient {
    async fn embed(
        &self,
        input: &[String],
        _kind: EmbeddingInputKind,
        cancel: CancellationToken,
    ) -> Result<Vec<Vec<f32>>, AiError> {
        if input.is_empty() {
            return Ok(Vec::new());
        }
        let operation_start = Instant::now();

        let batch_size = self.batch_size.max(1);
        let mut chunks = input.chunks(batch_size);
        let Some(first_chunk) = chunks.next() else {
            return Ok(Vec::new());
        };

        if cancel.is_cancelled() {
            return Err(AiError::Cancelled);
        }

        let remaining = self.timeout.saturating_sub(operation_start.elapsed());
        if remaining == Duration::ZERO {
            return Err(AiError::Timeout);
        }

        if self.embed_endpoint.load(Ordering::Acquire) != 2 {
            let first = match with_retry(
                "ollama",
                remaining,
                &self.retry,
                &cancel,
                |timeout| self.embed_via_embed_endpoint(first_chunk, timeout),
            )
            .await
            {
                Ok(first) => first,
                Err(err) => {
                    if matches!(err, AiError::Cancelled | AiError::Timeout) {
                        return Err(err);
                    }

                    tracing::warn!(
                        target = "nova.ai",
                        ?err,
                        "Ollama /api/embed failed; falling back to /api/embeddings"
                    );

                    return self
                        .embed_via_legacy_endpoint(input, &cancel, operation_start, &self.retry)
                        .await;
                }
            };

            match (self.embed_endpoint.load(Ordering::Acquire), first) {
                // Still unknown and Ollama returned 404: cache "unsupported" and fall back.
                (0, None) => {
                    self.embed_endpoint.store(2, Ordering::Release);
                }
                // We previously saw `/api/embed` succeed, so 404 should be treated as an error.
                (1, None) => {
                    return Err(AiError::UnexpectedResponse(
                        "ollama /api/embed returned 404 after a successful request".into(),
                    ));
                }
                (_, Some(embeddings)) => {
                    self.embed_endpoint.store(1, Ordering::Release);

                    let mut out = Vec::with_capacity(input.len());
                    out.extend(embeddings);

                    let mut chunks = chunks;
                    while let Some(chunk) = chunks.next() {
                        if cancel.is_cancelled() {
                            return Err(AiError::Cancelled);
                        }

                        let remaining = self.timeout.saturating_sub(operation_start.elapsed());
                        if remaining == Duration::ZERO {
                            return Err(AiError::Timeout);
                        }

                        let embeddings = match with_retry(
                            "ollama",
                            remaining,
                            &self.retry,
                            &cancel,
                            |timeout| self.embed_via_embed_endpoint(chunk, timeout),
                        )
                        .await
                        {
                            Ok(Some(embeddings)) => embeddings,
                            Ok(None) => {
                                return Err(AiError::UnexpectedResponse(
                                    "ollama /api/embed returned 404 after a successful request"
                                        .into(),
                                ));
                            }
                            Err(err) => {
                                if matches!(err, AiError::Cancelled | AiError::Timeout) {
                                    return Err(err);
                                }

                                tracing::warn!(
                                    target = "nova.ai",
                                    ?err,
                                    "Ollama /api/embed failed; falling back to /api/embeddings"
                                );

                                out.extend(
                                    self.embed_via_legacy_endpoint(
                                        chunk,
                                        &cancel,
                                        operation_start,
                                        &self.retry,
                                    )
                                    .await?,
                                );

                                while let Some(chunk) = chunks.next() {
                                    out.extend(
                                        self.embed_via_legacy_endpoint(
                                            chunk,
                                            &cancel,
                                            operation_start,
                                            &self.retry,
                                        )
                                        .await?,
                                    );
                                }

                                return Ok(out);
                            }
                        };
                        out.extend(embeddings);
                    }

                    return Ok(out);
                }
                _ => {}
            }
        }

        self.embed_via_legacy_endpoint(input, &cancel, operation_start, &self.retry)
            .await
    }
}

#[derive(Debug, Deserialize)]
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

#[derive(Debug, Deserialize)]
struct OllamaEmbeddingsResponse {
    #[serde(default)]
    embedding: Vec<f32>,
}
