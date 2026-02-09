use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::AiError;
use nova_config::{AiConfig, AiProviderKind};

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

            Ok(Box::new(AzureOpenAiEmbeddingsClient::new(
                config.provider.url.clone(),
                api_key,
                deployment,
                api_version,
                timeout,
            )?))
        }
        AiProviderKind::OpenAi => {
            let api_key = config
                .api_key
                .clone()
                .ok_or_else(|| AiError::InvalidConfig("OpenAI embeddings require ai.api_key".into()))?;
            Ok(Box::new(OpenAiCompatibleEmbeddingsClient::new(
                config.provider.url.clone(),
                model.clone(),
                Some(api_key),
                timeout,
            )?))
        }
        AiProviderKind::OpenAiCompatible => Ok(Box::new(OpenAiCompatibleEmbeddingsClient::new(
            config.provider.url.clone(),
            model,
            config.api_key.clone(),
            timeout,
        )?)),
        other => Err(AiError::InvalidConfig(format!(
            "ai.provider.kind = {other:?} does not support provider-backed embeddings"
        ))),
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
