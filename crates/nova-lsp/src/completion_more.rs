use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{atomic::{AtomicU64, Ordering}, mpsc, Mutex};

use lsp_types::CompletionList;

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

enum AiState {
    Pending(mpsc::Receiver<Vec<lsp_types::CompletionItem>>),
    Disabled,
}

struct CompletionSession {
    ai_state: AiState,
}

/// Runtime-agnostic completion service used by the LSP integration.
///
/// The base completion list is returned immediately and AI multi-token
/// completions are computed asynchronously in a background thread. Because LSP
/// completion results are not streamed, clients can poll `nova/completion/more`
/// with the returned `context_id`.
pub struct NovaCompletionService {
    engine: CompletionEngine,
    next_id: AtomicU64,
    sessions: Mutex<HashMap<CompletionContextId, CompletionSession>>,
}

impl NovaCompletionService {
    pub fn new(engine: CompletionEngine) -> Self {
        Self {
            engine,
            next_id: AtomicU64::new(1),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    pub fn completion_engine(&self) -> &CompletionEngine {
        &self.engine
    }

    /// Equivalent to `textDocument/completion` for the multi-token completion prototype.
    pub fn completion(&self, ctx: MultiTokenCompletionContext) -> NovaCompletionResponse {
        let context_id = CompletionContextId(self.next_id.fetch_add(1, Ordering::Relaxed));

        let standard_items = self.engine.standard_completions(&ctx);
        let standard_insert_texts: HashSet<String> =
            standard_items.iter().map(|item| item.insert_text.clone()).collect();

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
            let (tx, rx) = mpsc::channel();
            let engine = self.engine.clone();
            let ctx_clone = ctx.clone();
            let context_id_clone = context_id;

            std::thread::spawn(move || {
                let ai_items = engine.ai_completions(&ctx_clone);

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

            self.sessions.lock().expect("poisoned mutex").insert(
                context_id,
                CompletionSession {
                    ai_state: AiState::Pending(rx),
                },
            );
        } else {
            self.sessions.lock().expect("poisoned mutex").insert(
                context_id,
                CompletionSession {
                    ai_state: AiState::Disabled,
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
            AiState::Disabled => {
                sessions.remove(&context_id);
                MoreCompletionsResult {
                    items: Vec::new(),
                    is_incomplete: false,
                }
            }
            AiState::Pending(rx) => match rx.try_recv() {
                Ok(items) => {
                    sessions.remove(&context_id);
                    MoreCompletionsResult {
                        items,
                        is_incomplete: false,
                    }
                }
                Err(mpsc::TryRecvError::Empty) => MoreCompletionsResult {
                    items: Vec::new(),
                    is_incomplete: true,
                },
                Err(mpsc::TryRecvError::Disconnected) => {
                    sessions.remove(&context_id);
                    MoreCompletionsResult {
                        items: Vec::new(),
                        is_incomplete: false,
                    }
                }
            },
        }
    }
}

