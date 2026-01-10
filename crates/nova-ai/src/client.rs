use crate::{
    llm_privacy::PrivacyFilter,
    providers::{ollama::OllamaProvider, openai_compatible::OpenAiCompatibleProvider, AiProvider},
    types::{AiStream, ChatRequest, CodeSnippet},
    AiError,
};
use futures::StreamExt;
use nova_config::{AiConfig, AiProviderKind};
use url::Host;
use std::sync::Arc;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

pub struct AiClient {
    provider: Arc<dyn AiProvider>,
    semaphore: Arc<Semaphore>,
    privacy: PrivacyFilter,
    default_max_tokens: u32,
}

impl AiClient {
    pub fn from_config(config: &AiConfig) -> Result<Self, AiError> {
        if config.provider.concurrency == 0 {
            return Err(AiError::InvalidConfig(
                "ai.provider.concurrency must be >= 1".into(),
            ));
        }

        if config.privacy.local_only {
            validate_local_only_url(&config.provider.url)?;
        }

        let provider: Arc<dyn AiProvider> = match config.provider.kind {
            AiProviderKind::Ollama => Arc::new(OllamaProvider::new(
                config.provider.url.clone(),
                config.provider.model.clone(),
                config.provider.timeout(),
            )?),
            AiProviderKind::OpenAiCompatible => Arc::new(OpenAiCompatibleProvider::new(
                config.provider.url.clone(),
                config.provider.model.clone(),
                config.provider.timeout(),
                config.api_key.clone(),
            )?),
        };

        Ok(Self {
            provider,
            semaphore: Arc::new(Semaphore::new(config.provider.concurrency)),
            privacy: PrivacyFilter::new(&config.privacy)?,
            default_max_tokens: config.provider.max_tokens,
        })
    }

    pub fn sanitize_snippet(&self, snippet: &CodeSnippet) -> Option<String> {
        self.privacy.sanitize_snippet(snippet)
    }

    pub async fn chat(
        &self,
        mut request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<String, AiError> {
        if request.max_tokens.is_none() {
            request.max_tokens = Some(self.default_max_tokens);
        }

        for message in &mut request.messages {
            message.content = self.privacy.sanitize_prompt_text(&message.content);
        }

        let _permit = self
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| AiError::UnexpectedResponse("ai client shutting down".into()))?;

        self.provider.chat(request, cancel).await
    }

    pub async fn chat_stream(
        &self,
        mut request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<AiStream, AiError> {
        if request.max_tokens.is_none() {
            request.max_tokens = Some(self.default_max_tokens);
        }

        for message in &mut request.messages {
            message.content = self.privacy.sanitize_prompt_text(&message.content);
        }

        let permit = self
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| AiError::UnexpectedResponse("ai client shutting down".into()))?;

        let inner = self.provider.chat_stream(request, cancel).await?;
        let stream = async_stream::try_stream! {
            let _permit = permit;
            let mut inner = inner;
            while let Some(item) = inner.next().await {
                yield item?;
            }
        };

        let stream: AiStream = Box::pin(stream);
        Ok(stream)
    }

    pub async fn list_models(&self, cancel: CancellationToken) -> Result<Vec<String>, AiError> {
        let _permit = self
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| AiError::UnexpectedResponse("ai client shutting down".into()))?;
        self.provider.list_models(cancel).await
    }
}

fn validate_local_only_url(url: &url::Url) -> Result<(), AiError> {
    let is_loopback = match url.host() {
        Some(Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    };

    if is_loopback {
        return Ok(());
    }

    Err(AiError::InvalidConfig(format!(
        "ai.privacy.local_only=true requires ai.provider.url to use a loopback host \
        (localhost/127.0.0.1/[::1]); got {url}"
    )))
}
