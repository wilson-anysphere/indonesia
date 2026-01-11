use crate::{
    audit,
    llm_privacy::PrivacyFilter,
    providers::{ollama::OllamaProvider, openai_compatible::OpenAiCompatibleProvider, AiProvider},
    types::{AiStream, ChatRequest, CodeSnippet},
    AiError,
};
use futures::StreamExt;
use nova_config::{AiConfig, AiProviderKind};
use url::Host;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

pub struct AiClient {
    provider: Arc<dyn AiProvider>,
    semaphore: Arc<Semaphore>,
    privacy: PrivacyFilter,
    default_max_tokens: u32,
    audit_enabled: bool,
    provider_label: &'static str,
    model: String,
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
            audit_enabled: config.enabled && config.audit_log.enabled,
            provider_label: provider_label(&config.provider.kind),
            model: config.provider.model.clone(),
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
            let sanitized = self.privacy.sanitize_prompt_text(&message.content);
            message.content = if self.audit_enabled {
                audit::sanitize_prompt_for_audit(&sanitized)
            } else {
                sanitized
            };
        }

        let _permit = self
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| AiError::UnexpectedResponse("ai client shutting down".into()))?;

        let prompt_for_log = if self.audit_enabled {
            Some(audit::format_chat_prompt(&request.messages))
        } else {
            None
        };

        let started_at = Instant::now();
        if let Some(prompt) = prompt_for_log.as_deref() {
            audit::log_llm_request(
                self.provider_label,
                &self.model,
                prompt,
                /*endpoint=*/ None,
                /*attempt=*/ 0,
                /*stream=*/ false,
            );
        }

        match self.provider.chat(request, cancel).await {
            Ok(completion) => {
                if self.audit_enabled {
                    audit::log_llm_response(
                        self.provider_label,
                        &self.model,
                        &completion,
                        started_at.elapsed(),
                        /*retry_count=*/ 0,
                        /*stream=*/ false,
                        /*chunk_count=*/ None,
                    );
                }
                Ok(completion)
            }
            Err(err) => {
                if self.audit_enabled {
                    audit::log_llm_error(
                        self.provider_label,
                        &self.model,
                        &err.to_string(),
                        started_at.elapsed(),
                        /*retry_count=*/ 0,
                        /*stream=*/ false,
                    );
                }
                Err(err)
            }
        }
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
            let sanitized = self.privacy.sanitize_prompt_text(&message.content);
            message.content = if self.audit_enabled {
                audit::sanitize_prompt_for_audit(&sanitized)
            } else {
                sanitized
            };
        }

        let permit = self
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| AiError::UnexpectedResponse("ai client shutting down".into()))?;

        let prompt_for_log = if self.audit_enabled {
            Some(audit::format_chat_prompt(&request.messages))
        } else {
            None
        };
        let started_at = Instant::now();
        if let Some(prompt) = prompt_for_log.as_deref() {
            audit::log_llm_request(
                self.provider_label,
                &self.model,
                prompt,
                /*endpoint=*/ None,
                /*attempt=*/ 0,
                /*stream=*/ true,
            );
        }

        let inner = match self.provider.chat_stream(request, cancel).await {
            Ok(stream) => stream,
            Err(err) => {
                if self.audit_enabled {
                    audit::log_llm_error(
                        self.provider_label,
                        &self.model,
                        &err.to_string(),
                        started_at.elapsed(),
                        /*retry_count=*/ 0,
                        /*stream=*/ true,
                    );
                }
                return Err(err);
            }
        };

        let audit_enabled = self.audit_enabled;
        let provider_label = self.provider_label;
        let model = self.model.clone();
        let started_at_for_stream = started_at;

        let stream = async_stream::try_stream! {
            let _permit = permit;
            let mut inner = inner;
            let mut completion = String::new();
            let mut chunk_count = 0usize;
            while let Some(item) = inner.next().await {
                match item {
                    Ok(chunk) => {
                        chunk_count += 1;
                        completion.push_str(&chunk);
                        yield chunk;
                    }
                    Err(err) => {
                        if audit_enabled {
                            audit::log_llm_error(
                                provider_label,
                                &model,
                                &err.to_string(),
                                started_at_for_stream.elapsed(),
                                /*retry_count=*/ 0,
                                /*stream=*/ true,
                            );
                        }
                        Err(err)?;
                    }
                }
            }

            if audit_enabled {
                audit::log_llm_response(
                    provider_label,
                    &model,
                    &completion,
                    started_at_for_stream.elapsed(),
                    /*retry_count=*/ 0,
                    /*stream=*/ true,
                    Some(chunk_count),
                );
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

fn provider_label(kind: &AiProviderKind) -> &'static str {
    match kind {
        AiProviderKind::Ollama => "ollama",
        AiProviderKind::OpenAiCompatible => "openai_compatible",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use futures::TryStreamExt;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tracing::{field::Visit, Event};
    use tracing_subscriber::{layer::Context, prelude::*, Layer};

    #[derive(Debug, Clone)]
    struct CapturedEvent {
        target: String,
        fields: HashMap<String, String>,
    }

    #[derive(Clone)]
    struct CapturingLayer {
        events: Arc<Mutex<Vec<CapturedEvent>>>,
    }

    impl<S> Layer<S> for CapturingLayer
    where
        S: tracing::Subscriber,
    {
        fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
            let mut visitor = FieldVisitor::default();
            event.record(&mut visitor);

            let captured = CapturedEvent {
                target: event.metadata().target().to_string(),
                fields: visitor.fields,
            };
            self.events
                .lock()
                .expect("events mutex poisoned")
                .push(captured);
        }
    }

    #[derive(Default)]
    struct FieldVisitor {
        fields: HashMap<String, String>,
    }

    impl Visit for FieldVisitor {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.fields
                .insert(field.name().to_string(), format!("{value:?}"));
        }

        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.fields
                .insert(field.name().to_string(), value.to_string());
        }

        fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
            self.fields
                .insert(field.name().to_string(), value.to_string());
        }

        fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
            self.fields
                .insert(field.name().to_string(), value.to_string());
        }

        fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
            self.fields
                .insert(field.name().to_string(), value.to_string());
        }
    }

    #[derive(Clone, Default)]
    struct DummyProvider;

    #[async_trait]
    impl AiProvider for DummyProvider {
        async fn chat(
            &self,
            request: ChatRequest,
            _cancel: CancellationToken,
        ) -> Result<String, AiError> {
            let prompt = audit::format_chat_prompt(&request.messages);
            assert!(
                prompt.contains("[REDACTED]"),
                "expected prompt to be sanitized before sending"
            );
            Ok("completion sk-012345678901234567890123456789".to_string())
        }

        async fn chat_stream(
            &self,
            request: ChatRequest,
            _cancel: CancellationToken,
        ) -> Result<AiStream, AiError> {
            let prompt = audit::format_chat_prompt(&request.messages);
            assert!(
                prompt.contains("[REDACTED]"),
                "expected prompt to be sanitized before sending"
            );

            let stream = async_stream::try_stream! {
                yield "chunk ".to_string();
                yield "sk-012345678901234567890123456789".to_string();
            };
            Ok(Box::pin(stream))
        }

        async fn list_models(&self, _cancel: CancellationToken) -> Result<Vec<String>, AiError> {
            Ok(vec!["dummy".to_string()])
        }
    }

    fn make_test_client(provider: Arc<dyn AiProvider>) -> AiClient {
        let privacy = PrivacyFilter::new(&nova_config::AiPrivacyConfig::default())
            .expect("default privacy config is valid");

        AiClient {
            provider,
            semaphore: Arc::new(Semaphore::new(1)),
            privacy,
            default_max_tokens: 128,
            audit_enabled: true,
            provider_label: "dummy",
            model: "dummy-model".to_string(),
        }
    }

    fn audit_events(events: &[CapturedEvent]) -> Vec<CapturedEvent> {
        events
            .iter()
            .filter(|event| event.target == nova_config::AI_AUDIT_TARGET)
            .cloned()
            .collect()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn chat_emits_audit_events_with_sanitized_content() {
        let events = Arc::new(Mutex::new(Vec::<CapturedEvent>::new()));
        let layer = CapturingLayer {
            events: events.clone(),
        };

        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        let client = make_test_client(Arc::new(DummyProvider));
        let secret = "sk-012345678901234567890123456789";

        let completion = client
            .chat(
                ChatRequest {
                    messages: vec![crate::types::ChatMessage::user(format!("hello {secret}"))],
                    max_tokens: None,
                },
                CancellationToken::new(),
            )
            .await
            .expect("chat succeeds");

        assert!(completion.contains(secret), "dummy returns unsanitized content");

        let events = events.lock().unwrap();
        let audit = audit_events(&events);

        let request = audit
            .iter()
            .find(|event| event.fields.get("event").map(String::as_str) == Some("llm_request"))
            .expect("request audit event emitted");
        let prompt = request.fields.get("prompt").expect("prompt field present");
        assert!(
            prompt.contains("[REDACTED]"),
            "expected prompt to be redacted in audit logs"
        );
        assert!(
            !prompt.contains(secret),
            "expected secret to be removed from audit prompt"
        );

        let response = audit
            .iter()
            .find(|event| event.fields.get("event").map(String::as_str) == Some("llm_response"))
            .expect("response audit event emitted");
        let completion = response
            .fields
            .get("completion")
            .expect("completion field present");
        assert!(
            completion.contains("[REDACTED]"),
            "expected completion to be redacted in audit logs"
        );
        assert!(
            !completion.contains(secret),
            "expected secret to be removed from audit completion"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn chat_stream_emits_final_audit_event_with_concatenated_completion() {
        let events = Arc::new(Mutex::new(Vec::<CapturedEvent>::new()));
        let layer = CapturingLayer {
            events: events.clone(),
        };

        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        let client = make_test_client(Arc::new(DummyProvider));
        let secret = "sk-012345678901234567890123456789";

        let stream = client
            .chat_stream(
                ChatRequest {
                    messages: vec![crate::types::ChatMessage::user(format!("hello {secret}"))],
                    max_tokens: None,
                },
                CancellationToken::new(),
            )
            .await
            .expect("stream starts");

        let parts: Vec<String> = stream.try_collect().await.expect("stream ok");
        assert_eq!(parts.concat(), format!("chunk {secret}"));

        let events = events.lock().unwrap();
        let audit = audit_events(&events);

        let response = audit
            .iter()
            .find(|event| event.fields.get("event").map(String::as_str) == Some("llm_response"))
            .expect("response audit event emitted");
        assert_eq!(
            response.fields.get("stream").map(String::as_str),
            Some("true"),
            "expected stream=true on response"
        );
        let completion = response
            .fields
            .get("completion")
            .expect("completion field present");
        assert!(completion.contains("[REDACTED]"));
        assert!(!completion.contains(secret));
    }
}
