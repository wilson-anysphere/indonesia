use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

use lsp_types::CompletionList;
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
            ai_concurrency: config.concurrency,
            ..Self::default()
        }
    }
}

impl Default for CompletionMoreConfig {
    fn default() -> Self {
        Self {
            ai_concurrency: 4,
            session_ttl: Duration::from_secs(30),
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

impl NovaCompletionService {
    pub fn new(engine: CompletionEngine) -> Self {
        Self::with_config(engine, CompletionMoreConfig::default())
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

    fn prune_expired_sessions_locked(
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
    }

    /// Equivalent to `textDocument/completion` for the multi-token completion prototype.
    pub fn completion(&self, ctx: MultiTokenCompletionContext) -> NovaCompletionResponse {
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

        let list = CompletionList {
            is_incomplete: supports_ai,
            items: lsp_items,
            ..CompletionList::default()
        };

        if supports_ai {
            let (tx, rx) = oneshot::channel();
            let engine = self.engine.clone();
            let ctx_clone = ctx.clone();
            let context_id_clone = context_id;
            let cancel = CancellationToken::new();
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

                let mut lsp_items: Vec<_> = ai_items
                    .into_iter()
                    .map(|item| to_lsp_completion_item(item, &context_id_clone))
                    .collect();

                lsp_items.retain(|item| {
                    item.insert_text
                        .as_deref()
                        .map(|text| !standard_insert_texts.contains(text))
                        .unwrap_or(true)
                });

                let _ = tx.send(lsp_items);
            });

            let mut sessions = self.sessions.lock().expect("poisoned mutex");
            self.prune_expired_sessions_locked(&mut sessions);
            sessions.insert(
                context_id,
                CompletionSession {
                    expires_at: Instant::now() + ttl,
                    cancel,
                    ai_state: AiState::Pending(rx),
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

        let mut sessions = self.sessions.lock().expect("poisoned mutex");
        self.prune_expired_sessions_locked(&mut sessions);
        let session = match sessions.get_mut(&context_id) {
            Some(session) => session,
            None => {
                return MoreCompletionsResult {
                    items: Vec::new(),
                    is_incomplete: false,
                }
            }
        };

        match &mut session.ai_state {
            AiState::Pending(rx) => match rx.try_recv() {
                Ok(items) => {
                    sessions.remove(&context_id);
                    MoreCompletionsResult {
                        items,
                        is_incomplete: false,
                    }
                }
                Err(oneshot::error::TryRecvError::Empty) => MoreCompletionsResult {
                    items: Vec::new(),
                    is_incomplete: true,
                },
                Err(oneshot::error::TryRecvError::Closed) => {
                    if let Some(session) = sessions.remove(&context_id) {
                        session.cancel();
                    }
                    MoreCompletionsResult {
                        items: Vec::new(),
                        is_incomplete: false,
                    }
                }
            },
        }
    }

    pub fn cancel(&self, context_id: CompletionContextId) -> bool {
        let mut sessions = self.sessions.lock().expect("poisoned mutex");
        self.prune_expired_sessions_locked(&mut sessions);
        match sessions.remove(&context_id) {
            Some(session) => {
                session.cancel();
                true
            }
            None => false,
        }
    }
}
