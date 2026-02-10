use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use httpmock::prelude::*;
use nova_ai::{AiClient, CancellationToken, ChatMessage, ChatRequest};
use nova_config::{AiConfig, AiProviderKind};
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

#[tokio::test(flavor = "current_thread")]
async fn ai_client_retry_warning_does_not_leak_provider_url_secrets() {
    let server = MockServer::start();
    let _mock = server.mock(|when, then| {
        when.method(POST)
            .path("/")
            .query_param("key", "supersecret");
        then.status(500);
    });

    let mut endpoint = url::Url::parse(&server.base_url()).expect("base url");
    endpoint
        .set_username("user")
        .expect("set url username");
    endpoint
        .set_password(Some("pass"))
        .expect("set url password");
    endpoint.set_query(Some("key=supersecret"));

    let mut config = AiConfig::default();
    config.provider.kind = AiProviderKind::Http;
    config.provider.url = endpoint;
    config.provider.model = "test-model".to_string();
    // Force at least one retry but keep it fast.
    config.provider.retry_max_retries = 1;
    config.provider.retry_initial_backoff_ms = 1;
    config.provider.retry_max_backoff_ms = 1;
    config.provider.timeout_ms = 1_000;

    let events = Arc::new(Mutex::new(Vec::<CapturedEvent>::new()));
    let layer = CapturingLayer {
        events: events.clone(),
    };
    let subscriber = tracing_subscriber::registry().with(layer);
    let _guard = tracing::subscriber::set_default(subscriber);

    let client = AiClient::from_config(&config).expect("ai client");
    let _ = client
        .chat(
            ChatRequest {
                messages: vec![ChatMessage::user("hello".to_string())],
                max_tokens: None,
                temperature: None,
            },
            CancellationToken::new(),
        )
        .await;

    let events = events.lock().expect("events mutex poisoned");
    let retry_events: Vec<_> = events
        .iter()
        .filter(|event| {
            event.fields.values().any(|value| {
                value.contains("llm request failed, retrying")
                    || value.contains("llm stream request failed, retrying")
            })
        })
        .collect();

    assert!(
        !retry_events.is_empty(),
        "expected retry warning event, captured: {events:?}"
    );

    for event in retry_events {
        for (field, value) in &event.fields {
            assert!(
                !value.contains("supersecret"),
                "event field `{field}` leaked query value: {value}"
            );
            assert!(
                !value.contains("key="),
                "event field `{field}` leaked query key: {value}"
            );
            assert!(
                !value.contains("user:pass@"),
                "event field `{field}` leaked url userinfo: {value}"
            );
        }
        assert!(
            event.target != nova_config::AI_AUDIT_TARGET,
            "retry warnings should be emitted to non-audit targets"
        );
    }
}

