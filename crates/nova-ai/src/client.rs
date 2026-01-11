use crate::{
    audit,
    cache::{shared_cache, CacheKeyBuilder, CacheSettings, LlmResponseCache},
    llm_privacy::PrivacyFilter,
    providers::{ollama::OllamaProvider, openai_compatible::OpenAiCompatibleProvider, AiProvider},
    types::{AiStream, ChatRequest, CodeSnippet},
    AiError,
};
use futures::StreamExt;
use nova_config::{AiConfig, AiProviderKind};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use url::Host;

#[cfg(feature = "local-llm")]
use crate::providers::in_process_llama::InProcessLlamaProvider;

pub struct AiClient {
    provider: Arc<dyn AiProvider>,
    semaphore: Arc<Semaphore>,
    privacy: PrivacyFilter,
    default_max_tokens: u32,
    audit_enabled: bool,
    provider_label: &'static str,
    model: String,
    endpoint: url::Url,
    cache: Option<Arc<LlmResponseCache>>,
}

impl AiClient {
    pub fn from_config(config: &AiConfig) -> Result<Self, AiError> {
        let concurrency = config.provider.effective_concurrency();
        if concurrency == 0 {
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
            AiProviderKind::InProcessLlama => {
                #[cfg(feature = "local-llm")]
                {
                    Arc::new(InProcessLlamaProvider::new(&config.provider)?)
                }
                #[cfg(not(feature = "local-llm"))]
                {
                    return Err(AiError::InvalidConfig(
                        "ai.provider.kind = \"in_process_llama\" requires building nova-ai with --features local-llm"
                            .into(),
                    ));
                }
            }
        };

        let cache = if config.cache_enabled {
            if config.cache_max_entries == 0 {
                return Err(AiError::InvalidConfig(
                    "ai.cache_max_entries must be >= 1".into(),
                ));
            }
            if config.cache_ttl_secs == 0 {
                return Err(AiError::InvalidConfig(
                    "ai.cache_ttl_secs must be > 0".into(),
                ));
            }

            Some(shared_cache(CacheSettings {
                max_entries: config.cache_max_entries,
                ttl: std::time::Duration::from_secs(config.cache_ttl_secs),
            }))
        } else {
            None
        };

        Ok(Self {
            provider,
            semaphore: Arc::new(Semaphore::new(concurrency)),
            privacy: PrivacyFilter::new(&config.privacy)?,
            default_max_tokens: config.provider.max_tokens,
            audit_enabled: config.enabled && config.audit_log.enabled,
            provider_label: provider_label(&config.provider.kind),
            model: config.provider.model.clone(),
            endpoint: config.provider.url.clone(),
            cache,
        })
    }

    pub fn sanitize_snippet(&self, snippet: &CodeSnippet) -> Option<String> {
        let mut session = self.privacy.new_session();
        self.privacy.sanitize_snippet(&mut session, snippet)
    }

    pub fn is_excluded_path(&self, path: &Path) -> bool {
        self.privacy.is_excluded(path)
    }

    pub async fn chat(
        &self,
        mut request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<String, AiError> {
        if cancel.is_cancelled() {
            return Err(AiError::Cancelled);
        }

        if request.max_tokens.is_none() {
            request.max_tokens = Some(self.default_max_tokens);
        }

        let mut session = self.privacy.new_session();
        for message in &mut request.messages {
            let sanitized = self
                .privacy
                .sanitize_prompt_text(&mut session, &message.content);
            message.content = if self.audit_enabled {
                audit::sanitize_prompt_for_audit(&sanitized)
            } else {
                sanitized
            };
        }

        let prompt_for_log = if self.audit_enabled {
            Some(audit::format_chat_prompt(&request.messages))
        } else {
            None
        };
        let request_id = if self.audit_enabled {
            audit::next_request_id()
        } else {
            0
        };
        let safe_endpoint = if self.audit_enabled {
            Some(audit::sanitize_url_for_log(&self.endpoint))
        } else {
            None
        };

        let cache_key = self.cache.as_ref().map(|_| {
            let mut builder = CacheKeyBuilder::new("ai_chat_v1");
            builder.push_str(self.provider_label);
            builder.push_str(self.endpoint.as_str());
            builder.push_str(&self.model);
            builder.push_u32(request.max_tokens.unwrap_or(self.default_max_tokens));
            // ChatRequest doesn't currently expose temperature; keep the key
            // future-proof by reserving the slot.
            builder.push_u32(0);
            builder.push_u64(
                request
                    .messages
                    .len()
                    .try_into()
                    .expect("message count should fit u64"),
            );
            for message in &request.messages {
                let role = match message.role {
                    crate::types::ChatRole::System => "system",
                    crate::types::ChatRole::User => "user",
                    crate::types::ChatRole::Assistant => "assistant",
                };
                builder.push_str(role);
                builder.push_str(&message.content);
            }
            builder.finish()
        });

        if let (Some(cache), Some(key)) = (&self.cache, cache_key) {
            if let Some(hit) = cache.get(key).await {
                if let Some(prompt) = prompt_for_log.as_deref() {
                    let started_at = Instant::now();
                    audit::log_llm_request(
                        request_id,
                        self.provider_label,
                        &self.model,
                        prompt,
                        safe_endpoint.as_deref(),
                        /*attempt=*/ 0,
                        /*stream=*/ false,
                    );
                    audit::log_llm_response(
                        request_id,
                        self.provider_label,
                        &self.model,
                        &hit,
                        started_at.elapsed(),
                        /*retry_count=*/ 0,
                        /*stream=*/ false,
                        /*chunk_count=*/ None,
                    );
                }
                return Ok(hit);
            }
        }

        let _permit = self
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| AiError::UnexpectedResponse("ai client shutting down".into()))?;

        let started_at = Instant::now();
        if let Some(prompt) = prompt_for_log.as_deref() {
            audit::log_llm_request(
                request_id,
                self.provider_label,
                &self.model,
                prompt,
                safe_endpoint.as_deref(),
                /*attempt=*/ 0,
                /*stream=*/ false,
            );
        }

        match self.provider.chat(request, cancel).await {
            Ok(completion) => {
                if let (Some(cache), Some(key)) = (&self.cache, cache_key) {
                    cache.insert(key, completion.clone()).await;
                }
                if self.audit_enabled {
                    audit::log_llm_response(
                        request_id,
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
                        request_id,
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

        let mut session = self.privacy.new_session();
        for message in &mut request.messages {
            let sanitized = self
                .privacy
                .sanitize_prompt_text(&mut session, &message.content);
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
        let request_id = if self.audit_enabled {
            audit::next_request_id()
        } else {
            0
        };
        let safe_endpoint = if self.audit_enabled {
            Some(audit::sanitize_url_for_log(&self.endpoint))
        } else {
            None
        };
        let started_at = Instant::now();
        if let Some(prompt) = prompt_for_log.as_deref() {
            audit::log_llm_request(
                request_id,
                self.provider_label,
                &self.model,
                prompt,
                safe_endpoint.as_deref(),
                /*attempt=*/ 0,
                /*stream=*/ true,
            );
        }

        let inner = match self.provider.chat_stream(request, cancel).await {
            Ok(stream) => stream,
            Err(err) => {
                if self.audit_enabled {
                    audit::log_llm_error(
                        request_id,
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
        let request_id_for_stream = request_id;
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
                                request_id_for_stream,
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
                    request_id_for_stream,
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
    use nova_config::AiPrivacyConfig;
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

    const SECRET: &str = "sk-proj-012345678901234567890123456789";

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
            Ok(format!("completion {SECRET}"))
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
                yield SECRET.to_string();
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
            endpoint: url::Url::parse("http://localhost").expect("valid url"),
            cache: None,
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
        let secret = SECRET;

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

        assert!(
            completion.contains(secret),
            "dummy returns unsanitized content"
        );

        let events = events.lock().unwrap();
        let audit = audit_events(&events);

        let request = audit
            .iter()
            .find(|event| event.fields.get("event").map(String::as_str) == Some("llm_request"))
            .expect("request audit event emitted");
        let request_id = request
            .fields
            .get("request_id")
            .expect("request_id field present")
            .to_string();
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
        assert_eq!(
            response.fields.get("request_id").map(String::as_str),
            Some(request_id.as_str()),
            "request_id should correlate request/response"
        );
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
        let secret = SECRET;

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
        let request_id = audit
            .iter()
            .find(|event| event.fields.get("event").map(String::as_str) == Some("llm_request"))
            .and_then(|event| event.fields.get("request_id"))
            .expect("request_id field present");
        assert_eq!(
            response.fields.get("request_id").map(String::as_str),
            Some(request_id.as_str()),
            "request_id should correlate request/response"
        );
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

    #[derive(Default)]
    struct CapturedRequest {
        request: Mutex<Option<ChatRequest>>,
    }

    struct CapturingProvider {
        captured: Arc<CapturedRequest>,
    }

    #[async_trait]
    impl AiProvider for CapturingProvider {
        async fn chat(
            &self,
            request: ChatRequest,
            _cancel: CancellationToken,
        ) -> Result<String, AiError> {
            *self.captured.request.lock().unwrap() = Some(request);
            Ok("ok".to_string())
        }

        async fn chat_stream(
            &self,
            request: ChatRequest,
            _cancel: CancellationToken,
        ) -> Result<AiStream, AiError> {
            *self.captured.request.lock().unwrap() = Some(request);
            let stream = async_stream::try_stream! {
                yield "ok".to_string();
            };
            Ok(Box::pin(stream))
        }

        async fn list_models(&self, _cancel: CancellationToken) -> Result<Vec<String>, AiError> {
            Ok(vec![])
        }
    }

    #[tokio::test]
    async fn chat_sanitization_is_stable_across_messages_and_snippets() {
        let captured = Arc::new(CapturedRequest::default());
        let provider: Arc<dyn AiProvider> = Arc::new(CapturingProvider {
            captured: captured.clone(),
        });

        let privacy_cfg = AiPrivacyConfig {
            local_only: false,
            anonymize: Some(true),
            excluded_paths: Vec::new(),
            redact_patterns: Vec::new(),
        };
        let privacy = PrivacyFilter::new(&privacy_cfg).expect("privacy filter");

        let client = AiClient {
            provider,
            semaphore: Arc::new(Semaphore::new(1)),
            privacy,
            default_max_tokens: 16,
            audit_enabled: false,
            provider_label: "dummy",
            model: "dummy-model".to_string(),
            endpoint: url::Url::parse("http://localhost").expect("valid url"),
            cache: None,
        };

        let request = ChatRequest {
            messages: vec![
                crate::types::ChatMessage::user(
                    "Snippet 1:\n```java\nimport java.util.List;\nclass Foo {\n  // secret token\n  java.util.List<String> list = null;\n}\n```\n",
                ),
                crate::types::ChatMessage::user("Snippet 2:\n```java\nFoo foo = null;\n```\n"),
            ],
            max_tokens: None,
        };

        let _ = client
            .chat(request, CancellationToken::new())
            .await
            .expect("chat");

        let req = captured
            .request
            .lock()
            .expect("captured request mutex poisoned")
            .take()
            .expect("provider should receive request");
        let msg1 = &req.messages[0].content;
        let msg2 = &req.messages[1].content;

        // Stdlib fully-qualified names should remain readable.
        assert!(msg1.contains("java.util.List"), "{msg1}");

        // Identifiers should be anonymized consistently across snippets.
        assert!(!msg1.contains("Foo"), "{msg1}");
        assert!(!msg2.contains("Foo"), "{msg2}");
        assert!(msg1.contains("class id_0"), "{msg1}");
        assert!(msg2.contains("id_0"), "{msg2}");

        // Comments should be stripped when anonymization is enabled.
        assert!(msg1.contains("// [REDACTED]"), "{msg1}");
        assert!(!msg1.contains("secret token"), "{msg1}");
    }
}
