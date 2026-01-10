use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use nova_ai::{
    AdditionalEdit, AiProviderError, CompletionContextBuilder, MultiTokenCompletion,
    MultiTokenCompletionContext, MultiTokenCompletionProvider, MultiTokenInsertTextFormat,
};
use nova_ide::{CompletionConfig, CompletionEngine};
use nova_lsp::{MoreCompletionsParams, NovaCompletionService};

#[derive(Default)]
struct Gate {
    ready: Mutex<bool>,
    cvar: Condvar,
}

impl Gate {
    fn wait(&self) {
        let mut ready = self.ready.lock().expect("poisoned mutex");
        while !*ready {
            ready = self.cvar.wait(ready).expect("poisoned mutex");
        }
    }

    fn release(&self) {
        let mut ready = self.ready.lock().expect("poisoned mutex");
        *ready = true;
        self.cvar.notify_all();
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
        prompt: String,
        _max_items: usize,
    ) -> BoxFuture<'a, Result<Vec<MultiTokenCompletion>, AiProviderError>> {
        self.prompts.lock().expect("poisoned mutex").push(prompt);
        let gate = Arc::clone(&self.gate);
        let response = self.response.lock().expect("poisoned mutex").clone();

        Box::pin(async move {
            gate.wait();
            Ok(response)
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

#[test]
fn completion_more_returns_multi_token_items_async() {
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
        // Duplicate of a standard completion; should be removed during merge.
        MultiTokenCompletion {
            label: "dup".into(),
            insert_text: "filter".into(),
            format: MultiTokenInsertTextFormat::PlainText,
            additional_edits: vec![],
            confidence: 0.8,
        },
        // Invalid: unknown top-level method, should be filtered by validation.
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

    let completion = service.completion(ctx());
    assert!(completion.list.items.iter().any(|item| item.label == "filter"));

    let first_poll = service.completion_more(MoreCompletionsParams {
        context_id: completion.context_id.to_string(),
    });
    assert!(first_poll.items.is_empty());
    assert!(first_poll.is_incomplete);

    provider.release();

    let mut resolved = None;
    for _ in 0..50 {
        let poll = service.completion_more(MoreCompletionsParams {
            context_id: completion.context_id.to_string(),
        });
        if !poll.is_incomplete {
            resolved = Some(poll);
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    let poll = resolved.expect("AI completions should resolve");
    assert_eq!(poll.items.len(), 1);
    assert!(poll.items[0]
        .insert_text
        .as_deref()
        .unwrap()
        .contains("filter("));
    assert_eq!(poll.items[0].insert_text_format, Some(lsp_types::InsertTextFormat::SNIPPET));

    // Prompt construction included important context.
    let prompts = provider.prompts();
    assert_eq!(prompts.len(), 1);
    assert!(prompts[0].contains("Receiver type: Stream<Person>"));
    assert!(prompts[0].contains("Expected type: List<String>"));
    assert!(prompts[0].contains("- filter"));
    assert!(prompts[0].contains("people.stream()."));
}
