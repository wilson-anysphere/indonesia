use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

use futures::future::BoxFuture;
use nova_ai::{
    AdditionalEdit, AiProviderError, CancellationToken, CompletionContextBuilder,
    MultiTokenCompletion, MultiTokenCompletionContext, MultiTokenCompletionProvider,
    MultiTokenCompletionRequest, MultiTokenInsertTextFormat,
};
use nova_ide::{CompletionConfig, CompletionEngine};
use nova_lsp::NovaCompletionService;

struct Gate {
    ready: AtomicBool,
    notify: tokio::sync::Notify,
}

impl Default for Gate {
    fn default() -> Self {
        Self {
            ready: AtomicBool::new(false),
            notify: tokio::sync::Notify::new(),
        }
    }
}

impl Gate {
    async fn wait(&self) {
        while !self.ready.load(Ordering::SeqCst) {
            self.notify.notified().await;
        }
    }

    fn release(&self) {
        self.ready.store(true, Ordering::SeqCst);
        self.notify.notify_one();
    }
}

struct MockProvider {
    prompts: Mutex<Vec<String>>,
    gate: Arc<Gate>,
    response: Mutex<Vec<MultiTokenCompletion>>,
}

impl MockProvider {
    fn new() -> Self {
        Self {
            prompts: Mutex::new(Vec::new()),
            gate: Arc::new(Gate::default()),
            response: Mutex::new(Vec::new()),
        }
    }

    fn set_response(&self, completions: Vec<MultiTokenCompletion>) {
        *self.response.lock().expect("poisoned mutex") = completions;
    }

    fn release(&self) {
        self.gate.release();
    }

    fn prompts(&self) -> Vec<String> {
        self.prompts.lock().expect("poisoned mutex").clone()
    }
}

impl MultiTokenCompletionProvider for MockProvider {
    fn complete_multi_token<'a>(
        &'a self,
        request: MultiTokenCompletionRequest,
    ) -> BoxFuture<'a, Result<Vec<MultiTokenCompletion>, AiProviderError>> {
        let MultiTokenCompletionRequest { prompt, cancel, .. } = request;
        self.prompts.lock().expect("poisoned mutex").push(prompt);
        let gate = Arc::clone(&self.gate);
        let response = self.response.lock().expect("poisoned mutex").clone();

        Box::pin(async move {
            tokio::select! {
                _ = gate.wait() => Ok(response),
                _ = cancel.cancelled() => Err(AiProviderError::Cancelled),
            }
        })
    }
}

fn ctx() -> MultiTokenCompletionContext {
    MultiTokenCompletionContext {
        receiver_type: Some("Stream<Person>".into()),
        expected_type: Some("List<String>".into()),
        surrounding_code: "people.stream().".into(),
        available_methods: vec!["filter".into(), "map".into(), "collect".into()],
        importable_paths: vec!["java.util.stream.Collectors".into()],
    }
}

#[tokio::test]
async fn completion_more_returns_multi_token_items_async() {
    let provider = Arc::new(MockProvider::new());
    provider.set_response(vec![
        MultiTokenCompletion {
            label: "chain".into(),
            insert_text:
                "filter(${1:p} -> true).map(${2:Person}::getName).collect(${3:Collectors}.toList())"
                    .into(),
            format: MultiTokenInsertTextFormat::Snippet,
            additional_edits: vec![AdditionalEdit::AddImport {
                path: "java.util.stream.Collectors".into(),
            }],
            confidence: 0.9,
        },
        MultiTokenCompletion {
            label: "dup".into(),
            insert_text: "filter".into(),
            format: MultiTokenInsertTextFormat::PlainText,
            additional_edits: vec![],
            confidence: 0.8,
        },
        MultiTokenCompletion {
            label: "invalid".into(),
            insert_text: "unknown().map(x -> x)".into(),
            format: MultiTokenInsertTextFormat::PlainText,
            additional_edits: vec![],
            confidence: 0.5,
        },
    ]);

    let engine = CompletionEngine::new(
        CompletionConfig::default(),
        CompletionContextBuilder::new(10_000),
        Some(provider.clone()),
    );
    let service = NovaCompletionService::new(engine);

    let completion = service.completion(ctx(), CancellationToken::new());
    assert!(completion
        .list
        .items
        .iter()
        .any(|item| item.label == "filter"));

    let context_id = completion.context_id.to_string();
    let (first_items, first_incomplete) = service.completion_more(&context_id);
    assert!(first_items.is_empty());
    assert!(first_incomplete);

    provider.release();

    let mut resolved: Option<Vec<lsp_types::CompletionItem>> = None;
    for _ in 0..50 {
        let (items, incomplete) = service.completion_more(&context_id);
        if !incomplete {
            resolved = Some(items);
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let items = resolved.expect("AI completions should resolve");
    assert_eq!(items.len(), 1);
    assert!(items[0].insert_text.as_deref().unwrap().contains("filter("));
    assert_eq!(
        items[0].insert_text_format,
        Some(lsp_types::InsertTextFormat::SNIPPET)
    );

    let prompts = provider.prompts();
    assert_eq!(prompts.len(), 1);
    assert!(prompts[0].contains("Receiver type: Stream<Person>"));
    assert!(prompts[0].contains("Expected type: List<String>"));
    assert!(prompts[0].contains("- filter"));
    assert!(prompts[0].contains("people.stream()."));
}

#[tokio::test]
async fn completion_more_injects_document_uri_into_items() {
    let provider = Arc::new(MockProvider::new());
    provider.set_response(vec![MultiTokenCompletion {
        label: "chain".into(),
        insert_text: "filter(p -> true).collect(Collectors.toList())".into(),
        format: MultiTokenInsertTextFormat::PlainText,
        additional_edits: vec![AdditionalEdit::AddImport {
            path: "java.util.stream.Collectors".into(),
        }],
        confidence: 0.9,
    }]);
    provider.release();

    let engine = CompletionEngine::new(
        CompletionConfig::default(),
        CompletionContextBuilder::new(10_000),
        Some(provider),
    );
    let service = NovaCompletionService::new(engine);

    let uri = "file:///test/Completion.java".to_string();
    let completion =
        service.completion_with_document_uri(ctx(), CancellationToken::new(), Some(uri.clone()));
    let context_id = completion.context_id.to_string();

    for item in &completion.list.items {
        assert_eq!(
            item.data
                .as_ref()
                .and_then(|data| data.get("nova"))
                .and_then(|nova| nova.get("uri"))
                .and_then(|value| value.as_str()),
            Some(uri.as_str()),
            "standard completion items should carry the originating document URI"
        );
    }

    let mut resolved: Option<Vec<lsp_types::CompletionItem>> = None;
    for _ in 0..50 {
        let (items, incomplete) = service.completion_more(&context_id);
        if !incomplete {
            resolved = Some(items);
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let items = resolved.expect("AI completions should resolve");
    assert_eq!(items.len(), 1);
    assert_eq!(
        items[0]
            .data
            .as_ref()
            .and_then(|data| data.get("nova"))
            .and_then(|nova| nova.get("uri"))
            .and_then(|value| value.as_str()),
        Some(uri.as_str()),
        "AI completion items should carry the originating document URI"
    );
}

struct CancelAwareProvider {
    started: tokio::sync::Notify,
    finished: tokio::sync::Notify,
}

impl CancelAwareProvider {
    fn new() -> Self {
        Self {
            started: tokio::sync::Notify::new(),
            finished: tokio::sync::Notify::new(),
        }
    }

    async fn wait_started(&self) {
        self.started.notified().await;
    }

    async fn wait_finished(&self) {
        self.finished.notified().await;
    }
}

impl MultiTokenCompletionProvider for CancelAwareProvider {
    fn complete_multi_token<'a>(
        &'a self,
        request: MultiTokenCompletionRequest,
    ) -> BoxFuture<'a, Result<Vec<MultiTokenCompletion>, AiProviderError>> {
        let cancel = request.cancel;
        Box::pin(async move {
            self.started.notify_one();
            struct NotifyOnDrop<'a>(&'a tokio::sync::Notify);

            impl Drop for NotifyOnDrop<'_> {
                fn drop(&mut self) {
                    self.0.notify_one();
                }
            }

            let _guard = NotifyOnDrop(&self.finished);
            tokio::select! {
                _ = cancel.cancelled() => Err(AiProviderError::Cancelled),
                _ = tokio::time::sleep(Duration::from_secs(60)) => Ok(Vec::new()),
            }
        })
    }
}

#[tokio::test]
async fn completion_can_be_cancelled() {
    let provider = Arc::new(CancelAwareProvider::new());
    let mut config = CompletionConfig::default();
    config.ai_timeout_ms = 30_000;
    let engine = CompletionEngine::new(
        config,
        CompletionContextBuilder::new(10_000),
        Some(provider.clone()),
    );
    let service = NovaCompletionService::new(engine);

    let completion = service.completion(ctx(), CancellationToken::new());
    let context_id = completion.context_id.to_string();

    tokio::time::timeout(Duration::from_secs(1), provider.wait_started())
        .await
        .expect("provider should start");

    assert!(service.cancel(completion.context_id));

    tokio::time::timeout(Duration::from_secs(5), provider.wait_finished())
        .await
        .expect("provider should stop after cancellation");

    let (items, incomplete) = service.completion_more(&context_id);
    assert!(items.is_empty());
    assert!(!incomplete);
}

struct SlowProvider {
    started: tokio::sync::Notify,
}

impl SlowProvider {
    fn new() -> Self {
        Self {
            started: tokio::sync::Notify::new(),
        }
    }

    async fn wait_started(&self) {
        self.started.notified().await;
    }
}

impl MultiTokenCompletionProvider for SlowProvider {
    fn complete_multi_token<'a>(
        &'a self,
        _request: MultiTokenCompletionRequest,
    ) -> BoxFuture<'a, Result<Vec<MultiTokenCompletion>, AiProviderError>> {
        Box::pin(async move {
            self.started.notify_one();
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(Vec::new())
        })
    }
}

#[tokio::test]
async fn completion_times_out_and_returns_no_items() {
    let provider = Arc::new(SlowProvider::new());
    let mut config = CompletionConfig::default();
    config.ai_timeout_ms = 25;
    let engine = CompletionEngine::new(
        config,
        CompletionContextBuilder::new(10_000),
        Some(provider.clone()),
    );
    let service = NovaCompletionService::new(engine);

    let completion = service.completion(ctx(), CancellationToken::new());
    let context_id = completion.context_id.to_string();

    tokio::time::timeout(Duration::from_secs(1), provider.wait_started())
        .await
        .expect("provider should start");

    let mut resolved: Option<Vec<lsp_types::CompletionItem>> = None;
    for _ in 0..50 {
        let (items, incomplete) = service.completion_more(&context_id);
        if !incomplete {
            resolved = Some(items);
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let items = resolved.expect("AI completions should resolve (via timeout)");
    assert!(items.is_empty());
}
