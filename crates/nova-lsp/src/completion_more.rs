use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

use lsp_types::{CompletionList, CompletionItem, CompletionTextEdit, Range, TextEdit};
use tokio::sync::{oneshot, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::to_lsp::to_lsp_completion_item;
use crate::{MoreCompletionsParams, MoreCompletionsResult};
use nova_ai::MultiTokenCompletionContext;
use nova_ide::CompletionEngine;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CompletionContextId(u64);

impl fmt::Display for CompletionContextId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for CompletionContextId {
    type Err = std::num::ParseIntError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.parse()?))
    }
}

pub struct NovaCompletionResponse {
    pub context_id: CompletionContextId,
    pub list: CompletionList,
}

#[derive(Clone, Debug)]
pub struct CompletionMoreConfig {
    pub ai_concurrency: usize,
    pub session_ttl: Duration,
}

impl CompletionMoreConfig {
    pub fn from_provider_config(config: &nova_config::AiProviderConfig) -> Self {
        Self {
            ai_concurrency: config.effective_concurrency(),
            ..Self::default()
        }
    }
}

impl Default for CompletionMoreConfig {
    fn default() -> Self {
        Self {
            ai_concurrency: 4,
            session_ttl: Duration::from_secs(120),
        }
    }
}

enum AiState {
    Pending(oneshot::Receiver<Vec<lsp_types::CompletionItem>>),
}

struct CompletionSession {
    expires_at: Instant,
    cancel: CancellationToken,
    ai_state: AiState,
    last_access: Instant,
    document_uri: Option<String>,
    disallowed_insert_texts: HashSet<String>,
    prefix_range: Option<Range>,
}

impl CompletionSession {
    fn cancel(self) {
        self.cancel.cancel();
    }
}

/// Tokio-driven completion service used by the LSP integration.
///
/// The base completion list is returned immediately and AI multi-token
/// completions are computed asynchronously in the background. Because LSP
/// completion results are not streamed, clients can poll `nova/completion/more`
/// with the returned `context_id`.
pub struct NovaCompletionService {
    engine: CompletionEngine,
    config: CompletionMoreConfig,
    ai_semaphore: Arc<Semaphore>,
    next_id: AtomicU64,
    sessions: Mutex<HashMap<CompletionContextId, CompletionSession>>,
}

const MAX_COMPLETION_SESSIONS: usize = 256;

impl NovaCompletionService {
    pub fn new(engine: CompletionEngine) -> Self {
        Self::with_config(engine, CompletionMoreConfig::default())
    }

    /// Allocate a fresh completion context identifier.
    ///
    /// This is useful for LSP servers that want to tag completion items with a stable
    /// `completion_context_id` even when AI completions are disabled (the follow-up
    /// `nova/completion/more` request will simply return an empty result in that case).
    pub fn allocate_context_id(&self) -> CompletionContextId {
        CompletionContextId(self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    pub fn with_config(engine: CompletionEngine, config: CompletionMoreConfig) -> Self {
        let ai_concurrency = config.ai_concurrency.max(1);
        Self {
            engine,
            config,
            ai_semaphore: Arc::new(Semaphore::new(ai_concurrency)),
            next_id: AtomicU64::new(1),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    pub fn completion_engine(&self) -> &CompletionEngine {
        &self.engine
    }

    fn lock_sessions(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<CompletionContextId, CompletionSession>> {
        match self.sessions.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn prune_sessions_locked(
        &self,
        sessions: &mut HashMap<CompletionContextId, CompletionSession>,
    ) {
        let now = Instant::now();
        let expired: Vec<_> = sessions
            .iter()
            .filter_map(|(id, session)| (session.expires_at <= now).then_some(*id))
            .collect();

        for id in expired {
            if let Some(session) = sessions.remove(&id) {
                session.cancel();
            }
        }

        if sessions.len() <= MAX_COMPLETION_SESSIONS {
            return;
        }

        let mut by_age: Vec<_> = sessions
            .iter()
            .map(|(id, session)| (*id, session.last_access))
            .collect();
        by_age.sort_by_key(|(_, created_at)| *created_at);

        let overflow = sessions.len().saturating_sub(MAX_COMPLETION_SESSIONS);
        for (id, _) in by_age.into_iter().take(overflow) {
            if let Some(session) = sessions.remove(&id) {
                session.cancel();
            }
        }
    }

    /// Equivalent to `textDocument/completion` for the multi-token completion prototype.
    pub fn completion(
        &self,
        ctx: MultiTokenCompletionContext,
        cancel: CancellationToken,
    ) -> NovaCompletionResponse {
        self.completion_with_document_uri(ctx, cancel, None)
    }

    /// Start a completion session tied to a specific document URI.
    ///
    /// The LSP server uses this so that completions returned from `nova/completion/more` can be
    /// resolved via `completionItem/resolve` (which requires the originating document text to
    /// compute import edits).
    pub fn completion_with_document_uri(
        &self,
        ctx: MultiTokenCompletionContext,
        cancel: CancellationToken,
        document_uri: Option<String>,
    ) -> NovaCompletionResponse {
        let context_id = CompletionContextId(self.next_id.fetch_add(1, Ordering::Relaxed));

        let standard_items = self.engine.standard_completions(&ctx);
        let supports_ai = self.engine.supports_ai();

        let lsp_items: Vec<_> = standard_items
            .into_iter()
            .map(|item| to_lsp_completion_item(item, &context_id))
            .collect();

        let disallowed_insert_texts = build_disallowed_insert_texts(&lsp_items);

        let mut list = CompletionList {
            is_incomplete: supports_ai,
            items: lsp_items,
            ..CompletionList::default()
        };

        if let Some(uri) = document_uri.as_deref() {
            inject_uri_into_completion_items(&mut list.items, uri);
        }

        if supports_ai {
            let (tx, rx) = oneshot::channel();
            let engine = self.engine.clone();
            let ctx_clone = ctx.clone();
            let context_id_clone = context_id;
            let cancel = cancel.child_token();
            let cancel_task = cancel.clone();
            let permit_fut = Arc::clone(&self.ai_semaphore).acquire_owned();
            let ttl = self.config.session_ttl;

            tokio::spawn(async move {
                let permit = tokio::select! {
                    _ = cancel_task.cancelled() => return,
                    permit = permit_fut => match permit {
                        Ok(permit) => permit,
                        Err(_) => return,
                    },
                };

                let ai_items = engine
                    .ai_completions_async(&ctx_clone, cancel_task.clone())
                    .await;
                drop(permit);

                let lsp_items: Vec<_> = ai_items
                    .into_iter()
                    .map(|item| to_lsp_completion_item(item, &context_id_clone))
                    .collect();

                let _ = tx.send(lsp_items);
            });

            let mut sessions = self.lock_sessions();
            self.prune_sessions_locked(&mut sessions);
            let now = Instant::now();
            sessions.insert(
                context_id,
                CompletionSession {
                    expires_at: now + ttl,
                    cancel,
                    ai_state: AiState::Pending(rx),
                    last_access: now,
                    document_uri,
                    disallowed_insert_texts,
                    prefix_range: None,
                },
            );
        }

        NovaCompletionResponse { context_id, list }
    }

    /// Handle `nova/completion/more`.
    pub fn completion_more(&self, params: MoreCompletionsParams) -> MoreCompletionsResult {
        let context_id: CompletionContextId = match params.context_id.parse() {
            Ok(id) => id,
            Err(_) => {
                return MoreCompletionsResult {
                    items: Vec::new(),
                    is_incomplete: false,
                };
            }
        };

        let mut sessions = self.lock_sessions();
        self.prune_sessions_locked(&mut sessions);

        let mut session = match sessions.remove(&context_id) {
            Some(session) => session,
            None => {
                return MoreCompletionsResult {
                    items: Vec::new(),
                    is_incomplete: false,
                };
            }
        };

        session.last_access = Instant::now();

        let document_uri = session.document_uri.clone();
        match &mut session.ai_state {
            AiState::Pending(rx) => match rx.try_recv() {
                Ok(mut items) => {
                    nova_ai::filter_duplicates_against_insert_text_set(
                        &mut items,
                        &session.disallowed_insert_texts,
                        |item| Some(completion_item_effective_insert_text(item)),
                    );

                    if let Some(range) = session.prefix_range.clone() {
                        apply_prefix_text_edit(&mut items, range);
                    }

                    if let Some(uri) = document_uri.as_deref() {
                        inject_uri_into_completion_items(&mut items, uri);
                    }

                    session.cancel.cancel();

                    MoreCompletionsResult {
                        items,
                        is_incomplete: false,
                    }
                }
                Err(oneshot::error::TryRecvError::Empty) => {
                    sessions.insert(context_id, session);
                    MoreCompletionsResult {
                        items: Vec::new(),
                        is_incomplete: true,
                    }
                }
                Err(oneshot::error::TryRecvError::Closed) => {
                    session.cancel.cancel();
                    MoreCompletionsResult {
                        items: Vec::new(),
                        is_incomplete: false,
                    }
                }
            },
        }
    }

    pub fn cancel(&self, context_id: CompletionContextId) -> bool {
        let mut sessions = self.lock_sessions();
        self.prune_sessions_locked(&mut sessions);
        match sessions.remove(&context_id) {
            Some(session) => {
                session.cancel();
                true
            }
            None => false,
        }
    }

    /// Update the set of disallowed insert texts for an existing completion session.
    ///
    /// This is used by LSP integrations that compute their baseline completion list via the
    /// standard `textDocument/completion` pipeline (which may include extension-provided items,
    /// ranking, etc.) and want `nova/completion/more` to dedupe against the exact set of visible
    /// completions.
    pub fn register_disallowed_insert_texts(
        &self,
        context_id: CompletionContextId,
        disallowed_insert_texts: HashSet<String>,
    ) {
        let mut sessions = self.lock_sessions();
        self.prune_sessions_locked(&mut sessions);
        if let Some(session) = sessions.get_mut(&context_id) {
            session.disallowed_insert_texts = disallowed_insert_texts;
        }
    }

    /// Register the prefix replacement range for an existing completion session.
    ///
    /// AI multi-token completion items returned from `nova/completion/more` will use this range as
    /// their `textEdit` span, matching the base completion insertion semantics.
    pub fn register_prefix_range(&self, context_id: CompletionContextId, range: Range) {
        let mut sessions = self.lock_sessions();
        self.prune_sessions_locked(&mut sessions);
        if let Some(session) = sessions.get_mut(&context_id) {
            session.prefix_range = Some(range);
        }
    }
}

fn inject_uri_into_completion_items(items: &mut [lsp_types::CompletionItem], uri: &str) {
    for item in items {
        let Some(data) = item.data.as_mut().filter(|data| data.is_object()) else {
            continue;
        };
        if !data.get("nova").is_some_and(|nova| nova.is_object()) {
            data["nova"] = serde_json::json!({});
        }
        data["nova"]["uri"] = serde_json::json!(uri);
    }
}

fn completion_item_effective_insert_text(item: &CompletionItem) -> &str {
    match item.text_edit.as_ref() {
        Some(CompletionTextEdit::Edit(edit)) => edit.new_text.as_str(),
        Some(CompletionTextEdit::InsertAndReplace(edit)) => edit.new_text.as_str(),
        None => item
            .insert_text
            .as_deref()
            .unwrap_or_else(|| item.label.as_str()),
    }
}

fn build_disallowed_insert_texts(items: &[CompletionItem]) -> HashSet<String> {
    const MAX_DISALLOWED_INSERT_TEXTS: usize = 1024;

    let mut out = HashSet::new();
    for item in items {
        if out.len() >= MAX_DISALLOWED_INSERT_TEXTS {
            break;
        }
        let text = completion_item_effective_insert_text(item);
        if text.is_empty() {
            continue;
        }
        out.insert(text.to_string());
    }
    out
}

fn apply_prefix_text_edit(items: &mut [CompletionItem], range: Range) {
    for item in items {
        let new_text = completion_item_effective_insert_text(item).to_string();
        item.text_edit = Some(CompletionTextEdit::Edit(TextEdit {
            range: range.clone(),
            new_text,
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use futures::future::BoxFuture;
    use nova_ai::{
        AiProviderError, CompletionContextBuilder, MultiTokenCompletion, MultiTokenCompletionProvider,
        MultiTokenCompletionRequest, MultiTokenInsertTextFormat,
    };
    use nova_ide::{CompletionConfig, CompletionEngine};
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    };
    use std::time::Duration;

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
        gate: Arc<Gate>,
        response: Mutex<Vec<MultiTokenCompletion>>,
    }

    impl MockProvider {
        fn new() -> Self {
            Self {
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
    }

    impl MultiTokenCompletionProvider for MockProvider {
        fn complete_multi_token<'a>(
            &'a self,
            request: MultiTokenCompletionRequest,
        ) -> BoxFuture<'a, Result<Vec<MultiTokenCompletion>, AiProviderError>> {
            let cancel = request.cancel;
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
            importable_paths: vec![],
        }
    }

    #[tokio::test]
    async fn completion_more_filters_items_against_registered_disallowed_set() {
        let provider = Arc::new(MockProvider::new());
        provider.set_response(vec![
            MultiTokenCompletion {
                label: "dup".into(),
                insert_text: "filter(p -> true)".into(),
                format: MultiTokenInsertTextFormat::PlainText,
                additional_edits: vec![],
                confidence: 0.8,
            },
            MultiTokenCompletion {
                label: "unique".into(),
                insert_text: "map(x -> x)".into(),
                format: MultiTokenInsertTextFormat::PlainText,
                additional_edits: vec![],
                confidence: 0.7,
            },
        ]);

        let engine = CompletionEngine::new(
            CompletionConfig::default(),
            CompletionContextBuilder::new(10_000),
            Some(provider.clone()),
        );
        let service = NovaCompletionService::new(engine);

        let completion = service.completion(ctx(), CancellationToken::new());

        let mut disallowed = HashSet::new();
        disallowed.insert("filter(p -> true)".to_string());
        service.register_disallowed_insert_texts(completion.context_id, disallowed);

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
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let poll = resolved.expect("AI completions should resolve");
        assert_eq!(poll.items.len(), 1);
        assert_eq!(
            completion_item_effective_insert_text(&poll.items[0]),
            "map(x -> x)"
        );
    }
}
