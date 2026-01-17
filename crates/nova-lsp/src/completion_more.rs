use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

use lsp_types::CompletionList;
use serde_json::{Map, Value};
use tokio::sync::{oneshot, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::to_lsp::to_lsp_completion_item;
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
        crate::poison::lock(&self.sessions, "NovaCompletionService::lock_sessions")
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
        let standard_insert_texts: HashSet<String> = standard_items
            .iter()
            .map(|item| item.insert_text.clone())
            .collect();

        let supports_ai = self.engine.supports_ai();

        let lsp_items: Vec<_> = standard_items
            .into_iter()
            .map(|item| to_lsp_completion_item(item, &context_id))
            .collect();

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
                        Err(err) => {
                            tracing::debug!(
                                target = "nova.lsp",
                                context_id = %context_id_clone,
                                error = ?err,
                                "AI completion semaphore closed; skipping multi-token completions"
                            );
                            return;
                        }
                    },
                };

                let ai_items = engine
                    .ai_completions_async(&ctx_clone, cancel_task.clone())
                    .await;
                drop(permit);

                let mut lsp_items: Vec<_> = ai_items
                    .into_iter()
                    .map(|item| to_lsp_completion_item(item, &context_id_clone))
                    .collect();

                nova_ai::filter_duplicates_against_insert_text_set(
                    &mut lsp_items,
                    &standard_insert_texts,
                    |item| item.insert_text.as_deref(),
                );

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
                },
            );
        }

        NovaCompletionResponse { context_id, list }
    }

    /// Handle `nova/completion/more`.
    pub fn completion_more(&self, context_id: &str) -> (Vec<lsp_types::CompletionItem>, bool) {
        let context_id: CompletionContextId = match context_id.parse() {
            Ok(id) => id,
            Err(err) => {
                tracing::debug!(
                    target = "nova.lsp",
                    context_id,
                    error = %err,
                    "invalid completion context id"
                );
                return (Vec::new(), false);
            }
        };

        let mut sessions = self.lock_sessions();
        self.prune_sessions_locked(&mut sessions);
        let session = match sessions.get_mut(&context_id) {
            Some(session) => session,
            None => {
                return (Vec::new(), false);
            }
        };

        session.last_access = Instant::now();
        let document_uri = session.document_uri.clone();
        match &mut session.ai_state {
            AiState::Pending(rx) => match rx.try_recv() {
                Ok(mut items) => {
                    if let Some(session) = sessions.remove(&context_id) {
                        session.cancel();
                    }
                    if let Some(uri) = document_uri.as_deref() {
                        inject_uri_into_completion_items(&mut items, uri);
                    }
                    (items, false)
                }
                Err(oneshot::error::TryRecvError::Empty) => (Vec::new(), true),
                Err(oneshot::error::TryRecvError::Closed) => {
                    if let Some(session) = sessions.remove(&context_id) {
                        session.cancel();
                    }
                    (Vec::new(), false)
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
}

fn inject_uri_into_completion_items(items: &mut [lsp_types::CompletionItem], uri: &str) {
    for item in items {
        use crate::json_mut::{ensure_object_field_mut, ensure_object_mut};

        let data = item.data.get_or_insert_with(|| Value::Object(Map::new()));
        let Some(data) = ensure_object_mut(data) else {
            continue;
        };

        let Some(nova) = ensure_object_field_mut(data, "nova") else {
            continue;
        };
        nova.insert("uri".to_string(), Value::String(uri.to_string()));
    }
}
