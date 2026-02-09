use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::AiError;
use nova_config::{AiConfig, AiProviderKind};

use super::disk_cache::{DiskEmbeddingCache, EmbeddingCacheKey, DISK_CACHE_NAMESPACE_V1};
use super::EmbeddingsClient;

pub(super) fn provider_embeddings_client_from_config(
    config: &AiConfig,
) -> Result<Box<dyn EmbeddingsClient>, AiError> {
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

    let disk_cache = DiskEmbeddingCache::new(config.embeddings.model_dir.clone())
        .map(Arc::new)
        .ok();

    match &config.provider.kind {
        AiProviderKind::AzureOpenAi => {
            let api_key = config.api_key.clone().ok_or_else(|| {
                AiError::InvalidConfig("Azure OpenAI embeddings require ai.api_key".into())
            })?;
            let deployment = config.provider.azure_deployment.clone().ok_or_else(|| {
                AiError::InvalidConfig(
                    "Azure OpenAI embeddings require ai.provider.azure_deployment".into(),
                )
            })?;
            let api_version = config
                .provider
                .azure_api_version
                .clone()
                .unwrap_or_else(|| "2024-02-01".to_string());

            let base = AzureOpenAiEmbeddingsClient::new(
                config.provider.url.clone(),
                api_key,
                deployment,
                api_version,
                timeout,
            )?;
            let endpoint_id = base.endpoint_id()?;
            let cached = CachedEmbeddingsClient::new(
                Box::new(base),
                "azure_open_ai",
                endpoint_id,
                // Azure uses deployment-bound embeddings. Include it as the model id so caches don't
                // cross-contaminate deployments.
                cached_model_id_for_azure(config)?,
                disk_cache,
            );
            Ok(Box::new(cached))
        }
        AiProviderKind::OpenAi => {
            let api_key = config
                .api_key
                .clone()
                .ok_or_else(|| AiError::InvalidConfig("OpenAI embeddings require ai.api_key".into()))?;
            let base = OpenAiCompatibleEmbeddingsClient::new(
                config.provider.url.clone(),
                model.clone(),
                Some(api_key),
                timeout,
            )?;
            let endpoint_id = base.embeddings_endpoint_id()?;
            let cached = CachedEmbeddingsClient::new(
                Box::new(base),
                "openai",
                endpoint_id,
                model,
                disk_cache,
            );
            Ok(Box::new(cached))
        }
        AiProviderKind::OpenAiCompatible => {
            let base = OpenAiCompatibleEmbeddingsClient::new(
                config.provider.url.clone(),
                model.clone(),
                config.api_key.clone(),
                timeout,
            )?;
            let endpoint_id = base.embeddings_endpoint_id()?;
            let cached = CachedEmbeddingsClient::new(
                Box::new(base),
                "openai_compatible",
                endpoint_id,
                model,
                disk_cache,
            );
            Ok(Box::new(cached))
        }
        other => Err(AiError::InvalidConfig(format!(
            "ai.provider.kind = {other:?} does not support provider-backed embeddings"
        ))),
    }
}

fn cached_model_id_for_azure(config: &AiConfig) -> Result<String, AiError> {
    // `AzureOpenAiEmbeddingsClient` is keyed by deployment. If we don't have a deployment, the
    // config is already invalid; treat it as an error here so the cache key remains deterministic.
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
    memory_cache: Arc<Mutex<HashMap<EmbeddingCacheKey, Arc<Vec<f32>>>>>,
    disk_cache: Option<Arc<DiskEmbeddingCache>>,
}

impl CachedEmbeddingsClient {
    fn new(
        inner: Box<dyn EmbeddingsClient>,
        backend_id: &'static str,
        endpoint_id: String,
        model: String,
        disk_cache: Option<Arc<DiskEmbeddingCache>>,
    ) -> Self {
        Self {
            inner,
            backend_id,
            endpoint_id,
            model,
            memory_cache: Arc::new(Mutex::new(HashMap::new())),
            disk_cache,
        }
    }

    fn key_for(&self, input: &str) -> EmbeddingCacheKey {
        EmbeddingCacheKey::new(
            DISK_CACHE_NAMESPACE_V1,
            self.backend_id,
            &self.endpoint_id,
            &self.model,
            input.as_bytes(),
        )
    }
}

#[async_trait]
impl EmbeddingsClient for CachedEmbeddingsClient {
    async fn embed(
        &self,
        input: &[String],
        cancel: CancellationToken,
    ) -> Result<Vec<Vec<f32>>, AiError> {
        if input.is_empty() {
            return Ok(Vec::new());
        }
        if cancel.is_cancelled() {
            return Err(AiError::Cancelled);
        }

        let mut out = vec![None::<Vec<f32>>; input.len()];
        let mut miss_indices = Vec::new();
        let mut miss_keys = Vec::new();
        let mut miss_inputs = Vec::new();

        {
            let cache = self.memory_cache.lock().await;
            for (idx, text) in input.iter().enumerate() {
                let key = self.key_for(text);
                if let Some(hit) = cache.get(&key) {
                    out[idx] = Some((**hit).clone());
                } else {
                    miss_indices.push(idx);
                    miss_keys.push(key);
                    miss_inputs.push(text.clone());
                }
            }
        }

        // Disk cache lookups.
        if let Some(disk) = self.disk_cache.clone() {
            let disk_keys = miss_keys.clone();
            let expected = disk_keys.len();
            let disk_hits = match tokio::task::spawn_blocking(move || {
                disk_keys
                    .into_iter()
                    .map(|key| disk.load(key).ok().flatten())
                    .collect::<Vec<_>>()
            })
            .await
            {
                Ok(hits) if hits.len() == expected => hits,
                _ => vec![None; expected],
            };

            let mut still_indices = Vec::new();
            let mut still_keys = Vec::new();
            let mut still_inputs = Vec::new();
            let mut memory_inserts = Vec::new();

            for ((idx, key), hit) in miss_indices
                .into_iter()
                .zip(miss_keys.into_iter())
                .zip(disk_hits.into_iter())
            {
                if let Some(vec) = hit {
                    out[idx] = Some(vec.clone());
                    memory_inserts.push((key, vec));
                } else {
                    still_indices.push(idx);
                    still_keys.push(key);
                    // `miss_inputs` matched the same order as the keys.
                }
            }

            // Rebuild still_inputs based on still_indices to preserve ordering.
            for idx in &still_indices {
                if let Some(text) = input.get(*idx) {
                    still_inputs.push(text.clone());
                }
            }

            if !memory_inserts.is_empty() {
                let mut cache = self.memory_cache.lock().await;
                for (key, vec) in memory_inserts {
                    cache.insert(key, Arc::new(vec));
                }
            }

            miss_indices = still_indices;
            miss_keys = still_keys;
            miss_inputs = still_inputs;
        }

        // Network for cache misses.
        if !miss_inputs.is_empty() {
            let embeddings = self.inner.embed(&miss_inputs, cancel).await?;
            if embeddings.len() != miss_inputs.len() {
                return Err(AiError::UnexpectedResponse(format!(
                    "embedder returned unexpected batch size: expected {}, got {}",
                    miss_inputs.len(),
                    embeddings.len()
                )));
            }

            let mut memory_inserts = Vec::with_capacity(embeddings.len());
            let mut disk_inserts = Vec::with_capacity(embeddings.len());

            for ((orig_idx, key), embedding) in miss_indices
                .into_iter()
                .zip(miss_keys.into_iter())
                .zip(embeddings.into_iter())
            {
                out[orig_idx] = Some(embedding.clone());
                memory_inserts.push((key, embedding.clone()));
                disk_inserts.push((key, embedding));
            }

            if !memory_inserts.is_empty() {
                let mut cache = self.memory_cache.lock().await;
                for (key, vec) in memory_inserts {
                    cache.insert(key, Arc::new(vec));
                }
            }

            if let Some(disk) = self.disk_cache.clone() {
                let _ = tokio::task::spawn_blocking(move || {
                    for (key, vec) in disk_inserts {
                        let _ = disk.store(key, &vec);
                    }
                })
                .await;
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
    client: reqwest::Client,
}

impl AzureOpenAiEmbeddingsClient {
    fn new(
        endpoint: Url,
        api_key: String,
        deployment: String,
        api_version: String,
        timeout: Duration,
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
}

#[async_trait]
impl EmbeddingsClient for AzureOpenAiEmbeddingsClient {
    async fn embed(
        &self,
        input: &[String],
        cancel: CancellationToken,
    ) -> Result<Vec<Vec<f32>>, AiError> {
        if input.is_empty() {
            return Ok(Vec::new());
        }

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

        let fut = async {
            let response = self
                .client
                .post(url)
                .json(&body)
                .timeout(self.timeout)
                .send()
                .await?
                .error_for_status()?;
            let parsed: OpenAiEmbeddingsResponse = response.json().await?;
            parse_openai_embeddings(parsed, input.len())
        };

        tokio::select! {
            _ = cancel.cancelled() => Err(AiError::Cancelled),
            res = fut => res,
        }
    }
}

/// OpenAI-compatible embeddings provider.
#[derive(Clone)]
struct OpenAiCompatibleEmbeddingsClient {
    base_url: Url,
    model: String,
    timeout: Duration,
    api_key: Option<String>,
    client: reqwest::Client,
}

impl OpenAiCompatibleEmbeddingsClient {
    fn new(
        base_url: Url,
        model: String,
        api_key: Option<String>,
        timeout: Duration,
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
}

#[async_trait]
impl EmbeddingsClient for OpenAiCompatibleEmbeddingsClient {
    async fn embed(
        &self,
        input: &[String],
        cancel: CancellationToken,
    ) -> Result<Vec<Vec<f32>>, AiError> {
        if input.is_empty() {
            return Ok(Vec::new());
        }

        let url = self.endpoint("/embeddings")?;
        let body = json!({
            "model": &self.model,
            "input": input,
        });

        let fut = async {
            let mut request = self.client.post(url).json(&body).timeout(self.timeout);
            if let Some(key) = self.api_key.as_deref() {
                request = request.bearer_auth(key);
            }

            let response = request.send().await?.error_for_status()?;
            let parsed: OpenAiEmbeddingsResponse = response.json().await?;
            parse_openai_embeddings(parsed, input.len())
        };

        tokio::select! {
            _ = cancel.cancelled() => Err(AiError::Cancelled),
            res = fut => res,
        }
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
